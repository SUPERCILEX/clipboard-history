use clipboard_history_core::protocol::Request;

pub fn handle_payload(request: &Request, control_data: &[u8]) {
    dbg!(request, control_data);
}
