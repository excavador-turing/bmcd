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
//! A hardware-free HAL, selected by the `stubbed` feature.
//!
//! Everything under here simulates the board in memory. Nothing touches
//! `/dev/gpiochip*`, `/sys` or the device tree, so a `stubbed` build runs on a
//! developer's laptop and in CI. The two modules mirror the signatures of
//! their real counterparts exactly; when a signature changes on one side it
//! has to change on the other, and the compiler says so.
//!
//! `mod serial` and `pub mod usbboot` used to be declared here. Both files
//! were lost long before this fork, and nothing referenced what they exported:
//! serial handling lives in `crate::serial_service` and USB boot in
//! `crate::usb_boot`, neither of which goes through the HAL. They are not
//! restored, because writing stubs for an interface with no callers would be
//! inventing an API rather than mirroring one.
mod pin_controller;
mod power_controller;

pub use pin_controller::*;
pub use power_controller::*;
