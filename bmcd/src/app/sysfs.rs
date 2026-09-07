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
//! Reading single-value attributes out of sysfs.
//!
//! Every class directory under `/sys` is a set of one-value text files, and
//! the rules for reading them are the same wherever they live, so they are
//! spelled out once here rather than in each module that walks a class.
use std::path::Path;
use std::str::FromStr;

/// Reads one sysfs attribute as trimmed text. A missing file, an unreadable
/// one, and an empty one are all `None`: sysfs answers a read with an error
/// for attributes the driver cannot supply right now, and that is a normal
/// state for a device, not a fault of ours.
pub async fn read_attribute_string(dir: &Path, attribute: &str) -> Option<String> {
    let value = tokio::fs::read_to_string(dir.join(attribute)).await.ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Reads one sysfs attribute and parses it. A value the kernel will not give
/// us and a value that will not parse are the same `None`, because a caller
/// can do nothing different about them.
pub async fn read_attribute<T: FromStr>(dir: &Path, attribute: &str) -> Option<T> {
    read_attribute_string(dir, attribute).await?.parse().ok()
}
