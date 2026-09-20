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
    fn validate(hash: &str, password: &str) -> Result<(), AuthenticationError> {
        if !pwhash::unix::verify(password, hash) {
            Err(AuthenticationError::IncorrectCredentials)
        } else {
            Ok(())
        }
    }
}
