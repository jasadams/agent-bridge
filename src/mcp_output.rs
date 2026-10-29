//! MCP response views. Persistence and CLI results retain their complete records.
use serde_json::{Map, Value, json};

/// Select fields without filling missing fields with null or changing non-object results.
fn fields(value: Value, names: &[&str]) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    let mut selected = Map::new();
    for name in names {
        if let Some(value) = object.remove(*name) {
            selected.insert((*name).to_owned(), value);
        }
    }
    // Preserve an unexpected response rather than hide an error or schema change.
    if selected.is_empty() {
        Value::Object(object)
    } else {
        Value::Object(selected)
    }
}

pub(super) fn write_result(tool: &str, mut value: Value, verbose: bool) -> Value {
    if verbose || value.get("error").is_some() {
        return value;
    }
    if tool == "update_board" {
        // The current snapshot is required to resolve a rejected stale revision.
        if value["conflict"] == true || value["applied"] != true {
            return value;
        }
        return match value["board"].get("revision") {
            Some(revision) => json!({"board":value["board"]["name"],"revision":revision}),
            None => value,
        };
    }

    let names: &[&str] = match tool {
        "send_message" | "acknowledge_message" => &["id", "state"],
        "subscribe" | "unsubscribe" => &["agent", "events", "agents", "persistent", "recovery"],
        "open_pairing" => &["open", "valid_for_seconds", "notifications"],
        "create_invite" => &[
            "link",
            "peer_id",
            "fingerprint",
            "expires_at",
            "notifications",
        ],
        "connect_peer" | "confirm_pairing" => &[
            "id",
            "peer",
            "role",
            "state",
            "expires_at",
            "verification_code",
            "local_confirmed",
            "remote_confirmed",
            "notifications",
        ],
        "cancel_pairing" => &["cancelled", "id"],
        "close_pairing" => &["open"],
        // The caller supplies the selected agent and requested state alongside the peer record.
        "share_session" | "unshare_session" => &["peer_id", "agent", "shared", "revoked"],
        "configure_network" => &["enabled", "lan", "listen"],
        _ => return value,
    };
    if let Some(notifications) = value.get_mut("notifications") {
        *notifications = fields(
            notifications.take(),
            &["notification_agent", "id", "valid_for_seconds"],
        );
    }
    fields(value, names)
}

pub(super) fn agents(value: Value, verbose: bool) -> Value {
    if verbose {
        return value;
    }
    let Value::Array(rows) = value else {
        return value;
    };
    Value::Array(
        rows.into_iter()
            .map(|row| {
                let remote = row.get("online").is_some();
                if remote {
                    fields(
                        row,
                        &[
                            "name",
                            "provider",
                            "qualified_name",
                            "peer_id",
                            "peer_alias",
                            "online",
                        ],
                    )
                } else {
                    fields(
                        row,
                        &[
                            "name",
                            "provider",
                            "display_name",
                            "qualified_name",
                            "enabled",
                            "stale_reason",
                        ],
                    )
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_message_receipts_do_not_grow_with_the_body_or_machine_metadata() {
        for tool in ["send_message", "acknowledge_message"] {
            let mut sizes = Vec::new();
            for length in [100, 50_000] {
                let value = json!({
                    "id":"message-id", "state":"acknowledged", "body":"x".repeat(length),
                    "source_machine":{"details":"y".repeat(length)},
                    "destination_machine":{"details":"z".repeat(length)}
                });
                let compact = write_result(tool, value.clone(), false);
                assert_eq!(compact, json!({"id":"message-id","state":"acknowledged"}));
                sizes.push(serde_json::to_vec(&compact).unwrap().len());
                assert_eq!(write_result(tool, value.clone(), true), value);
            }
            assert_eq!(sizes[0], sizes[1]);
            assert!(sizes[0] < 100);
        }
    }

    #[test]
    fn board_receipt_is_constant_size_and_verbose_keeps_the_snapshot() {
        for length in [100, 50_000] {
            let value = json!({"applied":true,"board":{
                "name":"main","revision":"revision-id","markdown":"x".repeat(length),"path":"/board.md"
            }});
            let compact = write_result("update_board", value.clone(), false);
            assert_eq!(compact, json!({"board":"main","revision":"revision-id"}));
            assert!(serde_json::to_vec(&compact).unwrap().len() < 100);
            assert_eq!(write_result("update_board", value.clone(), true), value);
        }
    }

    #[test]
    fn board_conflicts_keep_the_current_snapshot_even_without_verbose() {
        let conflict = json!({"applied":false,"conflict":true,"board":{
            "revision":"current","markdown":"current content"
        },"message":"Review the current snapshot and retry"});
        assert_eq!(
            write_result("update_board", conflict.clone(), false),
            conflict
        );
    }

    #[test]
    fn discovery_keeps_names_and_remote_routes_without_transport_metadata() {
        let local = json!([{
            "name":"codex-id","provider":"codex","enabled":true,
            "display_name":"Planner","qualified_name":"codex:Planner",
            "endpoint":"/private/socket","session":"session-id",
            "machine_name":"local","peer_id":"local-id","machine_display_name":"Local"
        }]);
        assert_eq!(
            agents(local.clone(), false),
            json!([{
                "name":"codex-id","provider":"codex","enabled":true,
                "display_name":"Planner","qualified_name":"codex:Planner"
            }])
        );
        assert_eq!(agents(local.clone(), true), local);

        let remote = json!([{
            "name":"claude-id","provider":"claude","peer_id":"remote-id",
            "peer_alias":"workstation","qualified_name":"workstation/claude-id","online":false,
            "machine_name":"remote","machine_display_name":"Remote","session":"session-id"
        }]);
        assert_eq!(
            agents(remote.clone(), false),
            json!([{
                "name":"claude-id","provider":"claude","peer_id":"remote-id",
                "peer_alias":"workstation","qualified_name":"workstation/claude-id","online":false
            }])
        );
        assert_eq!(agents(remote.clone(), true), remote);
    }

    #[test]
    fn pairing_keeps_the_link_and_verification_state_without_notification_session_details() {
        let value = json!({
            "link":"agent-bridge://connect/example","peer_id":"peer","fingerprint":"fingerprint",
            "expires_at":123,"nonce":"nonce","qr_svg":"x".repeat(50_000),
            "notifications":{"notification_agent":"agent","id":null,
                "valid_for_seconds":120,"provider":"codex","session":"private-session"}
        });
        let compact = write_result("create_invite", value.clone(), false);
        assert_eq!(
            compact,
            json!({
                "link":"agent-bridge://connect/example","peer_id":"peer","fingerprint":"fingerprint",
                "expires_at":123,"notifications":{"notification_agent":"agent","id":null,"valid_for_seconds":120}
            })
        );
        assert_eq!(write_result("create_invite", value.clone(), true), value);

        let value = json!({"id":"exchange","peer":"peer","state":"awaiting_confirmation",
            "verification_code":"123 456","local_confirmed":false,"remote_confirmed":true});
        for tool in ["connect_peer", "confirm_pairing"] {
            assert_eq!(write_result(tool, value.clone(), false), value);
        }
    }

    #[test]
    fn sharing_does_not_echo_other_sessions_grants() {
        let value = json!({"peer_id":"peer","revoked":false,"agent":"selected","shared":true,
            "grants":[{"provider":"codex","session":"other","permission":"send"}]});
        assert_eq!(
            write_result("share_session", value.clone(), false),
            json!({"peer_id":"peer","revoked":false,"agent":"selected","shared":true})
        );
        assert_eq!(write_result("unshare_session", value.clone(), true), value);
    }

    #[test]
    fn subscriptions_and_simple_write_tools_keep_action_receipts() {
        for tool in ["subscribe", "unsubscribe"] {
            let receipt = json!({"agent":"agent","events":[{"event":"agent.opened"}],
                "agents":[{"provider":"claude","session":"session","route":"route","display_name":"Agent"}],
                "persistent":false,"recovery":{"total":0,"truncated":false,"items":[]}});
            let mut full = receipt.clone();
            full["internal"] = json!("omit");
            assert_eq!(write_result(tool, full, false), receipt);
        }
        for (tool, value) in [
            ("cancel_pairing", json!({"cancelled":true,"id":"exchange"})),
            ("close_pairing", json!({"open":false})),
            (
                "configure_network",
                json!({"enabled":true,"lan":false,"listen":"address"}),
            ),
        ] {
            assert_eq!(write_result(tool, value.clone(), false), value);
        }
    }

    #[test]
    fn compact_event_receipts_keep_nonempty_recovery_diagnostics() {
        let recovery = json!({"total":17,"truncated":true,"items":(0..16).map(|index| {
            json!({"id":format!("control-{index}"),"state":"uncertain",
                "reconciliation_pending":false,"route_matches":true,"lease_available":false})
        }).collect::<Vec<_>>(),"instruction":"Inspect the ID and explicitly reconcile through the same broker."});
        for tool in ["subscribe", "unsubscribe"] {
            let full = json!({"agent":"route","events":[],"agents":[],"persistent":false,
                "recovery":recovery,"internal":"omit"});
            let compact = write_result(tool, full, false);
            assert_eq!(compact["recovery"], recovery);
            assert!(compact.get("internal").is_none());
        }
    }

    #[test]
    fn unexpected_or_error_results_are_preserved() {
        for value in [
            json!(null),
            json!({"error":"actionable error"}),
            json!({"unexpected":true}),
        ] {
            assert_eq!(write_result("send_message", value.clone(), false), value);
        }
    }

    #[test]
    fn newly_added_record_fields_do_not_expand_compact_receipts() {
        let value = json!({"id":"message","state":"queued","future_metadata":"x".repeat(50_000)});
        assert_eq!(
            write_result("send_message", value, false),
            json!({"id":"message","state":"queued"})
        );
    }
}
