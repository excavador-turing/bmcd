// Copyright 2026 Oleg Tsarev
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

/// Evidence that a connection presented a client certificate which this
/// daemon verified against the CA in `tls.client_ca`.
///
/// Recorded per CONNECTION, not per request: TLS is a property of the
/// connection, and a request cannot acquire one by saying so. The
/// authentication service reads it back through `conn_data`, which actix
/// populates from the callback registered in `main.rs` and which no request
/// header can influence.
///
/// Its presence means exactly one thing: something holding a certificate
/// signed by our CA opened this connection. It does not say who the human
/// is -- that is what the identity header is for, and the header is believed
/// only when this is present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedProxy {
    /// The certificate's subject, for the audit line. Nothing branches on it:
    /// the CA already decided whether to trust the holder, and re-deciding
    /// here on a string would be a second, weaker access control that
    /// disagrees with the first.
    pub subject: String,
}

impl VerifiedProxy {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
        }
    }
}
