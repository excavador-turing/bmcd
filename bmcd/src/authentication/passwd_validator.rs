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
use super::authentication_errors::AuthenticationError;

pub trait PasswordValidator {
    fn validate(hash: &str, password: &str) -> Result<(), AuthenticationError>;
}

pub struct UnixValidator {}

impl PasswordValidator for UnixValidator {
    /// Nothing derived from the submitted password is logged, at any level.
    ///
    /// This used to log `crypt(password, hash)` at debug. For a correct
    /// password that is the stored hash, which is only as secret as
    /// `/etc/shadow`. For a WRONG one it is a hash of whatever was typed --
    /// and what people type into the wrong login box is usually a password
    /// that is correct somewhere else. A log file is a much easier thing to
    /// read than `/etc/shadow`, and the board's logs are collected.
    ///
    /// It was debugging scaffolding for the validator itself, which is four
    /// lines and has tests. There is no question it answers that is worth
    /// writing a password derivative to disk for.
    ///
    /// A locked or empty entry (`!`, `*`, `!$6$...`) is a wrong password: the
    /// account cannot log in, and that is not a fault of the board. Any other
    /// hash `crypt` cannot read is [`AuthenticationError::CheckFailed`], so a
    /// broken `/etc/shadow` line is reported as that and not counted as a
    /// guess against whoever tried.
    fn validate(hash: &str, password: &str) -> Result<(), AuthenticationError> {
        if hash.is_empty() || hash.starts_with('!') || hash.starts_with('*') {
            return Err(AuthenticationError::IncorrectCredentials);
        }
        let computed = pwhash::unix::crypt(password, hash).map_err(|_| {
            AuthenticationError::CheckFailed("the stored hash is not in a known format".into())
        })?;
        // Constant time over the bytes, as `pwhash::unix::verify` does.
        let differ = computed.len() != hash.len()
            || computed
                .bytes()
                .zip(hash.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                != 0;
        if differ {
            Err(AuthenticationError::IncorrectCredentials)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_crypt_cannot_read_is_not_a_wrong_password() {
        assert_eq!(
            UnixValidator::validate("$9$nonsense$", "turing"),
            Err(AuthenticationError::CheckFailed(
                "the stored hash is not in a known format".into()
            ))
        );
    }

    #[test]
    fn a_locked_account_is_a_wrong_password() {
        for locked in ["", "!", "*", "!$6$saltsalt$abc"] {
            assert_eq!(
                UnixValidator::validate(locked, "turing"),
                Err(AuthenticationError::IncorrectCredentials),
                "{locked:?}"
            );
        }
    }

    #[test]
    fn right_and_wrong_passwords() {
        let hash = pwhash::sha512_crypt::hash_with("$6$saltsalt$", "turing").unwrap();
        assert_eq!(UnixValidator::validate(&hash, "turing"), Ok(()));
        assert_eq!(
            UnixValidator::validate(&hash, "nope"),
            Err(AuthenticationError::IncorrectCredentials)
        );
    }
}
