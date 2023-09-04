// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::intr_pins::IntrPin;

pub struct AtaCtrl {
    /// Interrupt pins
    ata0_pin: Option<Box<dyn IntrPin>>,
    ata1_pin: Option<Box<dyn IntrPin>>,

    /// Whether or not we should service guest commands
    paused: bool,
}

impl AtaCtrl {
    pub fn new() -> Self {
        Self {
            ata0_pin: None,
            ata1_pin: None,
            paused: false,
        }
    }

    pub fn attach_irq(&mut self, ata0_pin: Box<dyn IntrPin>, ata1_pin: Box<dyn IntrPin>) {
        self.ata0_pin = Some(ata0_pin);
        self.ata1_pin = Some(ata1_pin);
    }
}
