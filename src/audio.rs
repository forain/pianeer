use std::sync::{Arc, Mutex};

use jack::{AudioOut, Client, Control, NotificationHandler, ProcessHandler, ProcessScope};

use crate::sampler::SamplerState;

pub struct PianoProcessor {
    pub out_l: jack::Port<AudioOut>,
    pub out_r: jack::Port<AudioOut>,
    pub state: Arc<Mutex<SamplerState>>,
}

impl ProcessHandler for PianoProcessor {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        let out_l_slice: &mut [f32] = self.out_l.as_mut_slice(ps);
        let out_r_slice: &mut [f32] = self.out_r.as_mut_slice(ps);

        if let Ok(ref mut state) = self.state.try_lock() {
            state.process(out_l_slice, out_r_slice);
        } else {
            for s in out_l_slice.iter_mut() { *s = 0.0; }
            for s in out_r_slice.iter_mut() { *s = 0.0; }
        }

        Control::Continue
    }
}

pub struct Notifications;

impl NotificationHandler for Notifications {
    fn xrun(&mut self, _: &Client) -> Control {
        eprintln!("JACK xrun detected");
        Control::Continue
    }
}
