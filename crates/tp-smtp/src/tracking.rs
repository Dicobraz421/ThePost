//! Журнал доставки (SRS 5.2, CONTRACTS.md#DeliveryLog).

/// Message-ID письма без угловых скобок (заголовки письма; нет или не разобран — `None`).
pub(crate) fn message_id(raw: &[u8]) -> Option<String> {
    mail_parser::MessageParser::new()
        .parse_headers(raw)
        .and_then(|m| m.message_id().map(str::to_owned))
}
