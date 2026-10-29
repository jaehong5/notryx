use std::collections::HashSet;

pub struct RequestGate {
    owner: String,
    registration: u64,
    pair_attempt: u64,
    pairing: bool,
    sending: Option<u64>,
    send_attempt: u64,
    sessions: HashSet<String>,
}

#[derive(Clone)]
pub struct RequestSnapshot {
    owner: String,
    registration: u64,
}

pub struct PairSnapshot {
    request: RequestSnapshot,
    attempt: u64,
}

#[derive(Clone, Copy)]
pub struct SendSnapshot {
    registration: u64,
    attempt: u64,
}

impl RequestGate {
    pub fn new(owner: String) -> Self {
        Self {
            owner,
            registration: 0,
            pair_attempt: 0,
            pairing: false,
            sending: None,
            send_attempt: 0,
            sessions: HashSet::new(),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn pair_status(&self, owner: &str, pending: bool) -> Result<&'static str, &'static str> {
        self.snapshot(owner)?;
        if !self.pairing {
            Ok("none")
        } else if pending {
            Ok("pending_adoption")
        } else {
            Ok("in_progress")
        }
    }

    pub fn snapshot(&self, owner: &str) -> Result<RequestSnapshot, &'static str> {
        if owner != self.owner {
            return Err("stale_renderer_session");
        }
        Ok(RequestSnapshot {
            owner: owner.into(),
            registration: self.registration,
        })
    }

    pub fn check_queue(&self, owner: &str) -> Result<(), &'static str> {
        self.snapshot(owner)?;
        if self.pairing {
            return Err("pairing_in_progress");
        }
        Ok(())
    }

    pub fn validate(&self, snapshot: &RequestSnapshot) -> Result<(), &'static str> {
        if snapshot.owner != self.owner {
            return Err("stale_renderer_session");
        }
        if snapshot.registration != self.registration {
            return Err("stale_registration");
        }
        Ok(())
    }

    pub fn replace_owner(&mut self, owner: String) {
        self.owner = owner;
        self.sessions.clear();
    }

    pub fn open_session(&mut self, session: String) -> Result<(), &'static str> {
        if self.pairing {
            return Err("pairing_in_progress");
        }
        if !self.sessions.insert(session) {
            return Err("session_already_open");
        }
        Ok(())
    }

    pub fn close_session(&mut self, session: &str) -> Result<(), &'static str> {
        if !self.sessions.remove(session) {
            return Err("stale_session");
        }
        Ok(())
    }

    pub fn check_session(&self, session: &str) -> Result<(), &'static str> {
        if !self.sessions.contains(session) {
            return Err("stale_session");
        }
        Ok(())
    }

    pub fn begin_pair(&mut self, owner: &str) -> Result<PairSnapshot, &'static str> {
        let request = self.snapshot(owner)?;
        if self.sending.is_some() {
            return Err("transmission_in_progress");
        }
        if self.pairing {
            return Err("pairing_in_progress");
        }
        if !self.sessions.is_empty() {
            return Err("open_edit_sessions");
        }
        self.pair_attempt = self
            .pair_attempt
            .checked_add(1)
            .ok_or("registration_limit")?;
        self.pairing = true;
        Ok(PairSnapshot {
            request,
            attempt: self.pair_attempt,
        })
    }

    pub fn validate_pair(&self, snapshot: &PairSnapshot) -> Result<(), &'static str> {
        self.validate(&snapshot.request)?;
        if !self.pairing || snapshot.attempt != self.pair_attempt {
            return Err("stale_registration");
        }
        if !self.sessions.is_empty() {
            return Err("open_edit_sessions");
        }
        self.registration
            .checked_add(1)
            .ok_or("registration_limit")?;
        Ok(())
    }

    pub fn commit_pair(&mut self, snapshot: &PairSnapshot) -> Result<(), &'static str> {
        self.validate_pair(snapshot)?;
        self.registration += 1;
        self.pairing = false;
        Ok(())
    }

    pub fn fail_pair(&mut self, snapshot: &PairSnapshot) -> Result<(), &'static str> {
        if !self.pairing || snapshot.attempt != self.pair_attempt {
            return Err("stale_registration");
        }
        self.pairing = false;
        Ok(())
    }

    pub fn adopt_pair(&self, owner: &str) -> Result<PairSnapshot, &'static str> {
        let request = self.snapshot(owner)?;
        if !self.pairing {
            return Err("no_pending_pair");
        }
        if !self.sessions.is_empty() {
            return Err("open_edit_sessions");
        }
        Ok(PairSnapshot {
            request,
            attempt: self.pair_attempt,
        })
    }

    pub fn begin_send(&mut self, owner: &str) -> Result<SendSnapshot, &'static str> {
        self.snapshot(owner)?;
        if self.pairing {
            return Err("pairing_in_progress");
        }
        if self.sending.is_some() {
            return Err("transmission_in_progress");
        }
        self.send_attempt = self
            .send_attempt
            .checked_add(1)
            .ok_or("transmission_limit")?;
        self.sending = Some(self.send_attempt);
        Ok(SendSnapshot {
            registration: self.registration,
            attempt: self.send_attempt,
        })
    }

    pub fn validate_send(&self, snapshot: &SendSnapshot) -> Result<(), &'static str> {
        if self.registration != snapshot.registration || self.sending != Some(snapshot.attempt) {
            return Err("stale_registration");
        }
        Ok(())
    }

    pub fn finish_send(&mut self, snapshot: &SendSnapshot) -> Result<(), &'static str> {
        self.validate_send(snapshot)?;
        self.sending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_reload_rejects_delayed_responses_and_clears_sessions() {
        let mut gate = RequestGate::new("owner-a".into());
        let request = gate.snapshot("owner-a").unwrap();
        gate.open_session("session-a".into()).unwrap();
        gate.replace_owner("owner-b".into());
        assert_eq!(gate.validate(&request), Err("stale_renderer_session"));
        assert_eq!(gate.close_session("session-a"), Err("stale_session"));
        assert!(gate.begin_pair("owner-b").is_ok());
    }

    #[test]
    fn pairing_requires_closed_sessions_and_cannot_apply_after_a_new_open() {
        let mut gate = RequestGate::new("owner".into());
        gate.open_session("session-a".into()).unwrap();
        assert!(matches!(
            gate.begin_pair("owner"),
            Err("open_edit_sessions")
        ));
        gate.close_session("session-a").unwrap();
        let pair = gate.begin_pair("owner").unwrap();
        assert_eq!(
            gate.open_session("session-b".into()),
            Err("pairing_in_progress")
        );
        gate.commit_pair(&pair).unwrap();
    }

    #[test]
    fn replacement_rejects_old_network_responses_and_competing_pair() {
        let mut gate = RequestGate::new("owner".into());
        let old = gate.snapshot("owner").unwrap();
        let first = gate.begin_pair("owner").unwrap();
        assert!(matches!(
            gate.begin_pair("owner"),
            Err("pairing_in_progress")
        ));
        gate.commit_pair(&first).unwrap();
        assert_eq!(gate.validate(&old), Err("stale_registration"));
    }

    #[test]
    fn failed_pair_releases_the_reservation() {
        let mut gate = RequestGate::new("owner".into());
        let first = gate.begin_pair("owner").unwrap();
        gate.fail_pair(&first).unwrap();
        gate.open_session("session".into()).unwrap();
        gate.close_session("session").unwrap();
        assert!(gate.begin_pair("owner").is_ok());
    }

    #[test]
    fn reloaded_renderer_can_explicitly_adopt_a_completed_pair() {
        let mut gate = RequestGate::new("old".into());
        let original = gate.begin_pair("old").unwrap();
        gate.replace_owner("new".into());
        assert_eq!(gate.validate_pair(&original), Err("stale_renderer_session"));
        assert_eq!(
            gate.open_session("editor".into()),
            Err("pairing_in_progress")
        );
        let adopted = gate.adopt_pair("new").unwrap();
        gate.commit_pair(&adopted).unwrap();
        gate.open_session("editor".into()).unwrap();
    }

    #[test]
    fn current_renderer_can_distinguish_pending_and_in_flight_pairing() {
        let mut gate = RequestGate::new("old".into());
        assert_eq!(gate.pair_status("old", false), Ok("none"));
        let pair = gate.begin_pair("old").unwrap();
        assert_eq!(gate.pair_status("old", false), Ok("in_progress"));
        gate.replace_owner("new".into());
        assert_eq!(gate.pair_status("old", true), Err("stale_renderer_session"));
        assert_eq!(gate.pair_status("new", true), Ok("pending_adoption"));
        let adopted = gate.adopt_pair("new").unwrap();
        gate.commit_pair(&adopted).unwrap();
        assert_eq!(gate.pair_status("new", false), Ok("none"));
        assert_eq!(gate.fail_pair(&pair), Err("stale_registration"));
    }

    #[test]
    fn sending_is_exclusive_with_pairing_but_not_renderer_reload() {
        let mut gate = RequestGate::new("owner".into());
        let send = gate.begin_send("owner").unwrap();
        assert!(matches!(
            gate.begin_send("owner"),
            Err("transmission_in_progress")
        ));
        assert!(matches!(
            gate.begin_pair("owner"),
            Err("transmission_in_progress")
        ));
        gate.replace_owner("new".into());
        assert_eq!(gate.validate_send(&send), Ok(()));
        gate.finish_send(&send).unwrap();
        let pair = gate.begin_pair("new").unwrap();
        assert!(matches!(gate.begin_send("new"), Err("pairing_in_progress")));
        gate.fail_pair(&pair).unwrap();
    }
}
