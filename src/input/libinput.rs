use smithay::backend::{input::InputEvent, libinput::LibinputInputBackend};

use crate::state::Pinnacle;

impl Pinnacle {
    /// Apply current libinput settings to new devices.
    pub fn apply_libinput_settings(&mut self, event: &InputEvent<LibinputInputBackend>) {
        let mut device = match event {
            InputEvent::DeviceAdded { device } => device.clone(),
            InputEvent::DeviceRemoved { device } => {
                self.input_state
                    .libinput_devices
                    .retain(|dev| dev != device);
                return;
            }
            _ => return,
        };

        if self.input_state.libinput_devices.contains(&device) {
            return;
        }

        for settings in self.input_state.libinput_settings.values() {
            for (filter, setting) in settings.iter().rev() {
                if filter.matches_device(&device) {
                    setting(&mut device);
                    break;
                }
            }
        }

        self.input_state.libinput_devices.push(device);
    }
}
