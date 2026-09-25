use rmux_proto::SessionName;

use super::PaneInputWrite;

pub(super) fn input_write_sessions(writes: &[PaneInputWrite]) -> Vec<SessionName> {
    let mut sessions = Vec::new();
    for write in writes {
        let session_name = write.session_name();
        if !sessions.iter().any(|existing| existing == session_name) {
            sessions.push(session_name.clone());
        }
    }
    sessions
}
