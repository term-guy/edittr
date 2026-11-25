//! LSP call hierarchy items carry opaque server data that must survive expansion.

use super::{Location, location_at, uri_to_path};
use crate::workspace;
use serde_json::Value;

pub(crate) struct Item {
    pub(crate) name: String,
    pub(crate) location: Location,
    pub(crate) wire: Value,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Direction {
    #[default]
    Incoming,
    Outgoing,
}

impl Direction {
    pub(super) fn method(self) -> &'static str {
        match self {
            Self::Incoming => "callHierarchy/incomingCalls",
            Self::Outgoing => "callHierarchy/outgoingCalls",
        }
    }

    pub(super) fn item_key(self) -> &'static str {
        match self {
            Self::Incoming => "from",
            Self::Outgoing => "to",
        }
    }
}

pub(super) fn supported(result: &Value) -> bool {
    let capability = &result["capabilities"]["callHierarchyProvider"];
    capability == true || capability.is_object()
}

fn parse_item(wire: Value) -> Option<Item> {
    let path = uri_to_path(wire["uri"].as_str()?)?;
    Some(Item {
        name: wire["name"].as_str()?.to_owned(),
        location: location_at(workspace::plain_path(&path), &wire["selectionRange"])?,
        wire,
    })
}

pub(super) fn parse_roots(value: Value) -> Vec<Item> {
    let Value::Array(items) = value else { return Vec::new() };
    items.into_iter().filter_map(parse_item).collect()
}

pub(super) fn parse_calls(value: Value, direction: Direction) -> Vec<Item> {
    let Value::Array(calls) = value else { return Vec::new() };
    calls
        .into_iter()
        .filter_map(|mut call| parse_item(call.get_mut(direction.item_key())?.take()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(name: &str) -> Value {
        let uri = super::super::path_to_uri(&std::env::temp_dir().join("test calls.rs"));
        json!({
            "name": name, "uri": uri, "kind": 12,
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 11, "character": 1}},
            "selectionRange": {"start": {"line": 6, "character": 3}, "end": {"line": 6, "character": 9}},
            "data": {"id": ["server", 42]}
        })
    }

    #[test]
    fn prepare_uses_selection_range_and_preserves_the_wire_item() {
        let wire = item("root");
        let roots = parse_roots(json!([wire, {"name": "broken"}, {"uri": "untitled:untitled-1"}]));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "root");
        assert!(roots[0].location.path.ends_with("test calls.rs"));
        assert_eq!((roots[0].location.line, roots[0].location.col16.clone()), (6, 3..9));
        assert_eq!(roots[0].wire, wire);
        assert!(parse_roots(Value::Null).is_empty());
    }

    #[test]
    fn calls_select_the_endpoint_for_each_direction() {
        let calls = json!([{"from": item("caller"), "to": item("callee"), "fromRanges": []}]);
        assert_eq!(parse_calls(calls.clone(), Direction::Incoming)[0].name, "caller");
        assert_eq!(parse_calls(calls, Direction::Outgoing)[0].name, "callee");
        assert!(parse_calls(json!([{}, null]), Direction::Outgoing).is_empty());
        assert!(parse_calls(Value::Null, Direction::Incoming).is_empty());
    }

    #[test]
    #[ignore = "requires clangd 20 or later on PATH"]
    fn lsp_round_trip_call_hierarchy_clangd() {
        use super::super::{Client, resolve, servers_for};
        use std::{fs, time::Duration};

        let dir = std::env::temp_dir().join(format!("edittr-call-hierarchy-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dir = workspace::canonical(&dir).unwrap();
        let path = dir.join("main.c");
        let text = "int leaf(void) { return 1; }\nint middle(void) { return leaf(); }\nint main(void) { return middle(); }\n";
        fs::write(&path, text).unwrap();

        let spec = &servers_for("c")[0];
        let client = Client::start(spec, resolve(spec.command).expect("clangd is not installed"), &dir, &[], None).unwrap();
        let timeout = Duration::from_secs(10);
        let roots = client.prepare_call_hierarchy(&path, text, (0, 0), (1, 5), timeout).unwrap();
        let [middle] = roots.as_slice() else { panic!("expected one root") };
        assert_eq!(middle.name, "middle");
        for (direction, expected, line) in [(Direction::Incoming, "main", 2), (Direction::Outgoing, "leaf", 0)] {
            let calls = client.hierarchy_calls(middle.wire.clone(), direction, timeout).unwrap();
            let [call] = calls.as_slice() else { panic!("expected one call") };
            assert_eq!((call.name.as_str(), call.location.line), (expected, line));
            assert_eq!(call.location.path, path);
        }
        drop(client);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn static_capability_accepts_both_lsp_shapes() {
        for provider in [json!(true), json!({}), json!({"workDoneProgress": true})] {
            assert!(supported(&json!({"capabilities": {"callHierarchyProvider": provider}})));
        }
        assert!(!supported(&json!({"capabilities": {"callHierarchyProvider": false}})));
        assert!(!supported(&json!({"capabilities": {}})));
    }
}
