use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Parse a KDL config string into a deserializable type by converting KDL → JSON → T.
pub fn parse_kdl_config<T: DeserializeOwned>(content: &str) -> Result<T, String> {
    let doc: kdl::KdlDocument = content.parse().map_err(|e| format!("KDL parse error: {e}"))?;
    let json_value = kdl_document_to_json(&doc);
    serde_json::from_value(json_value).map_err(|e| format!("config deserialization error: {e}"))
}

fn kdl_document_to_json(doc: &kdl::KdlDocument) -> Value {
    nodes_to_json(doc.nodes())
}

fn nodes_to_json(nodes: &[kdl::KdlNode]) -> Value {
    let mut map = Map::new();

    for node in nodes {
        let key = node.name().value().to_string();
        let value = node_to_json(node);

        // Repeated keys are accumulated into an array.
        if let Some(existing) = map.get_mut(&key) {
            match existing {
                Value::Array(arr) => arr.push(value),
                _ => {
                    let prev = existing.clone();
                    *existing = Value::Array(vec![prev, value]);
                }
            }
        } else {
            map.insert(key, value);
        }
    }

    Value::Object(map)
}

fn node_to_json(node: &kdl::KdlNode) -> Value {
    let has_children = node
        .children()
        .map_or(false, |c| !c.nodes().is_empty());
    let args: Vec<_> = node.entries().iter().filter(|e| e.name().is_none()).collect();
    let props: Vec<_> = node.entries().iter().filter(|e| e.name().is_some()).collect();

    if has_children {
        let children_doc = node.children().unwrap();
        let all_dash = children_doc
            .nodes()
            .iter()
            .all(|n| n.name().value() == "-");

        if all_dash && !children_doc.nodes().is_empty() {
            // All children named "-" → array
            let arr: Vec<Value> = children_doc
                .nodes()
                .iter()
                .map(|n| dash_node_to_json(n))
                .collect();
            return Value::Array(arr);
        }

        // Object: merge children + inline properties
        let mut obj = match kdl_document_to_json(children_doc) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        for prop in &props {
            obj.insert(
                prop.name().unwrap().value().to_string(),
                kdl_value_to_json(prop.value()),
            );
        }
        return Value::Object(obj);
    }

    // Leaf node
    if args.len() == 1 && props.is_empty() {
        return kdl_value_to_json(args[0].value());
    }

    if args.len() > 1 && props.is_empty() {
        return Value::Array(args.iter().map(|a| kdl_value_to_json(a.value())).collect());
    }

    if !props.is_empty() {
        let mut obj = Map::new();
        for prop in &props {
            obj.insert(
                prop.name().unwrap().value().to_string(),
                kdl_value_to_json(prop.value()),
            );
        }
        for arg in &args {
            // Positional args with properties: store under "_"
            if let Some(existing) = obj.get_mut("_") {
                match existing {
                    Value::Array(arr) => arr.push(kdl_value_to_json(arg.value())),
                    _ => {
                        let prev = existing.clone();
                        *existing = Value::Array(vec![prev, kdl_value_to_json(arg.value())]);
                    }
                }
            } else {
                obj.insert("_".to_string(), kdl_value_to_json(arg.value()));
            }
        }
        return Value::Object(obj);
    }

    Value::Null
}

/// Convert a "-" (dash) node into a JSON value for array elements.
fn dash_node_to_json(node: &kdl::KdlNode) -> Value {
    let args: Vec<_> = node.entries().iter().filter(|e| e.name().is_none()).collect();
    let props: Vec<_> = node.entries().iter().filter(|e| e.name().is_some()).collect();
    let has_children = node
        .children()
        .map_or(false, |c| !c.nodes().is_empty());

    if has_children {
        let mut obj = match kdl_document_to_json(node.children().unwrap()) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        for prop in &props {
            obj.insert(
                prop.name().unwrap().value().to_string(),
                kdl_value_to_json(prop.value()),
            );
        }
        return Value::Object(obj);
    }

    if args.len() == 1 && props.is_empty() {
        return kdl_value_to_json(args[0].value());
    }

    if !props.is_empty() {
        let mut obj = Map::new();
        for prop in props {
            obj.insert(
                prop.name().unwrap().value().to_string(),
                kdl_value_to_json(prop.value()),
            );
        }
        return Value::Object(obj);
    }

    if args.len() > 1 {
        return Value::Array(args.iter().map(|a| kdl_value_to_json(a.value())).collect());
    }

    Value::Null
}

fn kdl_value_to_json(value: &kdl::KdlValue) -> Value {
    match value {
        kdl::KdlValue::RawString(s) | kdl::KdlValue::String(s) => Value::String(s.clone()),
        kdl::KdlValue::Base2(i)
        | kdl::KdlValue::Base8(i)
        | kdl::KdlValue::Base10(i)
        | kdl::KdlValue::Base16(i) => Value::Number((*i).into()),
        kdl::KdlValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        kdl::KdlValue::Bool(b) => Value::Bool(*b),
        kdl::KdlValue::Null => Value::Null,
    }
}

/// Returns `true` if the file path has a `.kdl` extension.
pub fn is_kdl_file(path: &std::path::Path) -> bool {
    path.extension().map_or(false, |ext| ext == "kdl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Simple {
        name: String,
        port: i64,
        enabled: bool,
    }

    #[test]
    fn parse_flat_kdl() {
        let kdl = r#"
name "test"
port 8080
enabled true
"#;
        let result: Simple = parse_kdl_config(kdl).unwrap();
        assert_eq!(
            result,
            Simple {
                name: "test".to_string(),
                port: 8080,
                enabled: true,
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Nested {
        bridge: Bridge,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Bridge {
        domain: String,
        port: i64,
    }

    #[test]
    fn parse_nested_kdl() {
        let kdl = r#"
bridge {
    domain "example.org"
    port 9005
}
"#;
        let result: Nested = parse_kdl_config(kdl).unwrap();
        assert_eq!(
            result,
            Nested {
                bridge: Bridge {
                    domain: "example.org".to_string(),
                    port: 9005,
                },
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct WithArray {
        items: Vec<String>,
    }

    #[test]
    fn parse_array_with_dash_nodes() {
        let kdl = r#"
items {
    - "one"
    - "two"
    - "three"
}
"#;
        let result: WithArray = parse_kdl_config(kdl).unwrap();
        assert_eq!(
            result,
            WithArray {
                items: vec!["one".to_string(), "two".to_string(), "three".to_string()],
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct WithObjectArray {
        entries: Vec<Entry>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Entry {
        exclusive: bool,
        regex: String,
    }

    #[test]
    fn parse_array_of_objects() {
        let kdl = r#"
entries {
    - exclusive=true regex="@_bot_.*"
    - exclusive=false regex="@_user_.*"
}
"#;
        let result: WithObjectArray = parse_kdl_config(kdl).unwrap();
        assert_eq!(result.entries.len(), 2);
        assert!(result.entries[0].exclusive);
        assert_eq!(result.entries[0].regex, "@_bot_.*");
    }
}
