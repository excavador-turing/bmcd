// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use super::{
    authentication_context::AuthenticationContext, authentication_service::AuthenticationService,
    passwd_validator::UnixValidator,
};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use futures::StreamExt;
use inotify::WatchMask;
use inotify::{EventMask, Inotify};
use std::{
    future::{ready, Ready},
    io,
    path::Path,
    time::Duration,
};
use std::{rc::Rc, sync::Arc};
use tokio::{
    fs::OpenOptions,
    io::{AsyncBufReadExt, BufReader},
    sync::Mutex,
};

// One name for this file, in `factory_password`, because three copies of a
// path are three chances to change two of them.
use super::factory_password::SHADOW_FILE;

type LinuxContext = AuthenticationContext<UnixValidator>;

#[derive(Clone)]
pub struct LinuxAuthenticator {
    context: Arc<Mutex<LinuxContext>>,
    /// The board's own shadow file, asked on every request whether `root` is
    /// still `turing`. Built once and shared, because it caches on the file's
    /// modification time.
    factory: Arc<crate::authentication::factory_password::FactoryPassword>,
    authentication_path: &'static str,
    realm: &'static str,
    /// Which header names the human, when a verified client certificate
    /// makes it worth believing. Configured, because the proxy in front
    /// decides what it sets and that is not ours to hard-code.
    identity_header: std::sync::Arc<str>,
}

impl LinuxAuthenticator {
    pub async fn new(
        authentication_path: &'static str,
        realm: &'static str,
        authentication_token_duration: Duration,
        authentication_attemps: usize,
        identity_header: impl AsRef<str>,
    ) -> io::Result<Self> {
        let password_entries = Self::parse_shadow_file().await?;

        let mut context = LinuxContext::with_unix_validator(
            password_entries,
            authentication_token_duration,
            authentication_attemps,
        );
        // A new local token on every start. If it cannot be written the
        // on-board `tpi` falls back to logging in; nothing else depends on it.
        let local_token = super::local_token::generate();
        match super::local_token::write(
            std::path::Path::new(super::local_token::LOCAL_TOKEN_DIR),
            &local_token,
        ) {
            Ok(path) => tracing::info!("local token written to {}", path.display()),
            Err(e) => tracing::warn!(
                "could not write the local token to {}: {e}",
                super::local_token::LOCAL_TOKEN_DIR
            ),
        }
        context.set_local_token(local_token);

        let instance = Self {
            context: Arc::new(Mutex::new(context)),
            factory: Arc::new(
                crate::authentication::factory_password::FactoryPassword::new(SHADOW_FILE),
            ),
            authentication_path,
            realm,
            identity_header: identity_header.as_ref().into(),
        };

        if let Err(e) = instance.auto_reload().await {
            tracing::warn!("auto reloading of password-cache disabled: {}", e);
        }

        Ok(instance)
    }

    pub async fn revoke_bearer(&self, token: &str) {
        self.context.lock().await.revoke_bearer(token);
    }

    pub async fn clear_sessions(&self) {
        self.context.lock().await.clear_sessions();
    }
}

impl LinuxAuthenticator {
    /// Watches for any changes in the shadow file and reloads the password
    /// cache when a change is detected.
    async fn auto_reload(&self) -> std::io::Result<()> {
        spawn_shadow_watcher(
            std::path::PathBuf::from(SHADOW_FILE),
            self.context.clone(),
            WATCH_RETRY_FIRST,
        );
        Ok(())
    }

    async fn parse_shadow_file() -> io::Result<impl Iterator<Item = (String, String)>> {
        parse_shadow_file_at(Path::new(SHADOW_FILE)).await
    }
}

/// First wait before a watch that could not be set up is tried again. It
/// doubles on each failure up to [`WATCH_RETRY_MAX`].
const WATCH_RETRY_FIRST: Duration = Duration::from_millis(200);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(30);

/// Keep the password cache in step with `path` for as long as the daemon runs.
///
/// `passwd` replaces the shadow file by renaming a new one over it, which
/// ends a watch on the old inode (`DELETE_SELF`, then `IGNORED`). The watcher
/// is then built again on whatever is at `path` now, and the cache reloaded
/// from it, because that new file is the change. If the file is not there yet,
/// or inotify fails, it retries with backoff instead of giving up: a watcher
/// that stops leaves password changes unnoticed until the daemon restarts.
fn spawn_shadow_watcher(
    path: std::path::PathBuf,
    context: Arc<Mutex<LinuxContext>>,
    first_retry: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut retry = first_retry;
        let mut failing = false;
        loop {
            let mut events = match watch(&path) {
                Ok(events) => events,
                Err(e) => {
                    if failing {
                        tracing::debug!("{} still cannot be watched: {e}", path.display());
                    } else {
                        tracing::warn!("{} cannot be watched, retrying: {e}", path.display());
                        failing = true;
                    }
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(WATCH_RETRY_MAX);
                    continue;
                }
            };
            if failing {
                tracing::info!("watching {} again", path.display());
                failing = false;
            }
            retry = first_retry;
            // Whatever changed while there was no watch is picked up here.
            reload(&path, &context).await;

            while let Some(event) = events.next().await {
                match event {
                    Ok(event)
                        if event
                            .mask
                            .intersects(EventMask::DELETE_SELF | EventMask::IGNORED) =>
                    {
                        break;
                    }
                    Ok(_) => reload(&path, &context).await,
                    Err(e) => {
                        tracing::warn!("{} watcher failed: {e}", path.display());
                        break;
                    }
                }
            }
        }
    })
}

fn watch(path: &Path) -> io::Result<inotify::EventStream<[u8; 256]>> {
    let inotify = Inotify::init()?;
    inotify
        .watches()
        .add(path, WatchMask::DELETE_SELF | WatchMask::CLOSE_WRITE)?;
    inotify.into_event_stream([0; 256])
}

async fn reload(path: &Path, context: &Mutex<LinuxContext>) {
    match parse_shadow_file_at(path).await {
        Ok(entries) => {
            context.lock().await.reload_password_cache(entries);
            tracing::info!("reloaded user cache");
        }
        Err(e) => tracing::error!("error parsing {}: {e}", path.display()),
    }
}

async fn parse_shadow_file_at(path: &Path) -> io::Result<impl Iterator<Item = (String, String)>> {
    let file = OpenOptions::new().read(true).open(path).await?;

    let mut password_hashes: Vec<(String, String)> = Vec::new();
    let mut read_buffer = BufReader::new(file);

    loop {
        let mut line = String::new();
        let bytes_read = read_buffer.read_line(&mut line).await?;
        if bytes_read == 0 {
            break;
        }

        let mut items = line.splitn(3, ':');
        let username = items.next();
        let password = items.next();
        let (Some(user), Some(pass)) = (username, password) else {
            break;
        };

        if !pass.starts_with('*') {
            password_hashes.push((user.to_string(), pass.to_string()));
            tracing::debug!("loaded user {user}");
        }
    }
    Ok(password_hashes.into_iter())
}

impl<S, B> Transform<S, ServiceRequest> for LinuxAuthenticator
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = AuthenticationService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(AuthenticationService::new(
            Rc::new(service),
            self.context.clone(),
            self.authentication_path,
            self.realm,
            self.identity_header.clone(),
            self.factory.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authentication::authentication_context::tests::build_test_context;

    fn shadow_line(user: &str, password: &str) -> String {
        let hash = pwhash::sha512_crypt::hash_with("$6$saltsalt$", password).unwrap();
        format!("{user}:{hash}:19000:0:99999:7:::\n")
    }

    /// `passwd`'s way: write a new file and rename it over the old one.
    fn replace(path: &Path, contents: &str) {
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, contents).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    /// A fresh peer for every try, so the misses while waiting never add up
    /// to a ban.
    async fn logs_in(context: &Mutex<LinuxContext>, user: &str, password: &str) -> bool {
        static PEER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let peer = format!(
            "peer-{}",
            PEER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let body = format!(r#"{{"username":"{user}","password":"{password}"}}"#);
        context
            .lock()
            .await
            .authenticate_request(&peer, body.as_bytes())
            .await
            .is_ok()
    }

    async fn eventually(context: &Mutex<LinuxContext>, user: &str, password: &str) -> bool {
        for _ in 0..200 {
            if logs_in(context, user, password).await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bmcd-shadow-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two renames in a row: the watch on the first replaced inode ends, and
    /// the second change is still noticed.
    #[tokio::test]
    async fn password_changes_keep_being_noticed_after_a_rename() {
        let dir = scratch("rename");
        let shadow = dir.join("shadow");
        std::fs::write(&shadow, shadow_line("root", "one")).unwrap();
        let context = Arc::new(Mutex::new(build_test_context([], [])));
        let watcher =
            spawn_shadow_watcher(shadow.clone(), context.clone(), Duration::from_millis(10));

        assert!(eventually(&context, "root", "one").await);
        replace(&shadow, &shadow_line("root", "two"));
        assert!(eventually(&context, "root", "two").await);
        replace(&shadow, &shadow_line("root", "three"));
        assert!(eventually(&context, "root", "three").await);
        assert!(!logs_in(&context, "root", "two").await);

        watcher.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The file is gone for a while. The watcher keeps trying and reloads
    /// once it is back.
    #[tokio::test]
    async fn a_missing_shadow_file_is_watched_once_it_returns() {
        let dir = scratch("missing");
        let shadow = dir.join("shadow");
        let context = Arc::new(Mutex::new(build_test_context([], [])));
        let watcher =
            spawn_shadow_watcher(shadow.clone(), context.clone(), Duration::from_millis(10));

        tokio::time::sleep(Duration::from_millis(50)).await;
        replace(&shadow, &shadow_line("root", "back"));
        assert!(eventually(&context, "root", "back").await);

        watcher.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
}
