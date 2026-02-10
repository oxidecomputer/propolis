// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Specifications for components that can be attached to a Propolis VM.
//!
//! Components are 'versionless' and can be added to any specification of any
//! format. Existing components must only change in backward-compatible ways.

pub mod backends;
pub mod board;
pub mod devices;
