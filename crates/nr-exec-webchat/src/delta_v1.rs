//! Port of open-sse/executors/chatgpt-web/deltaV1.js
//!
//! Stateful decoder for ChatGPT's compact `delta_encoding: v1` document stream.

use serde_json::Value;

const UNSAFE_POINTER_SEGMENTS: &[&str] = &["__proto__", "constructor", "prototype"];
const DELTA_OPERATIONS: &[&str] = &["add", "append", "patch", "replace"];

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub json: Option<Value>,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub struct IngestResult {
    pub events: Vec<SseEvent>,
    pub changed: bool,
    pub done: bool,
}

/// Stateful decoder for the compact `delta_encoding: v1` document stream.
pub struct ChatGptWebDeltaV1Decoder {
    pub document: Option<Value>,
    last_path: Option<String>,
    last_operation: Option<String>,
    pub stream_done: bool,
}

impl ChatGptWebDeltaV1Decoder {
    pub fn new() -> Self {
        Self {
            document: None,
            last_path: None,
            last_operation: None,
            stream_done: false,
        }
    }

    pub fn snapshot(&self) -> Option<Value> {
        self.document.clone()
    }

    pub fn ingest(&mut self, encoded_item: &str) -> Result<IngestResult, String> {
        let events = parse_chatgpt_web_encoded_item(encoded_item)?;
        let mut changed = false;

        for event in &events {
            if event.event == "delta_encoding" {
                if event.json.as_ref().and_then(|v| v.as_str()) != Some("v1") {
                    return Err(format!(
                        "Unsupported ChatGPT Web delta encoding: {}",
                        event.json.as_ref().map(|v| v.to_string()).unwrap_or_default()
                    ));
                }
                self.document = None;
                self.last_path = None;
                self.last_operation = None;
                self.stream_done = false;
                continue;
            }
            if event.event == "delta" {
                if let Some(json) = &event.json {
                    self.apply_delta(json)?;
                    changed = true;
                }
            }
            if event.done {
                self.stream_done = true;
            }
        }

        Ok(IngestResult {
            events,
            changed,
            done: self.stream_done,
        })
    }

    fn apply_delta(&mut self, value: &Value) -> Result<(), String> {
        if !value.is_object() {
            return Err("ChatGPT Web delta payload must be an object".to_string());
        }
        let obj = value.as_object().unwrap();

        let path = if obj.contains_key("p") {
            obj.get("p").and_then(|v| v.as_str()).map(String::from)
        } else {
            self.last_path.clone()
        };

        let operation = if obj.contains_key("o") {
            obj.get("o").and_then(|v| v.as_str()).map(|s| s.to_string())
        } else {
            self.last_operation.clone()
        };

        let path = path.ok_or_else(|| "ChatGPT Web delta requires a path".to_string())?;
        let operation = operation.ok_or_else(|| "ChatGPT Web delta requires an operation".to_string())?;

        if !DELTA_OPERATIONS.contains(&operation.as_str()) {
            return Err(format!("Unsupported delta operation: {}", operation));
        }

        self.last_path = Some(path.clone());
        self.last_operation = Some(operation.clone());

        let delta_value = obj.get("v").cloned();
        self.apply_at(&path, &operation, delta_value)
    }

    fn apply_at(&mut self, path: &str, operation: &str, value: Option<Value>) -> Result<(), String> {
        if let Some(ref v) = value {
            assert_safe_value(v)?;
        }

        let segments = pointer_segments(path)?;
        if segments.is_empty() {
            self.apply_at_root(operation, value)?;
            return Ok(());
        }

        let doc = self.document.as_mut().ok_or("Document not initialized")?;
        let parent = Self::navigate_to_parent(doc, &segments)?;
        let key = &segments[segments.len() - 1];

        if let Some(arr) = parent.as_array_mut() {
            Self::apply_to_array(arr, key, operation, value)?;
        } else if let Some(obj) = parent.as_object_mut() {
            Self::apply_to_object(obj, key, operation, value)?;
        } else {
            return Err(format!("ChatGPT Web delta path is not mutable: {}", path));
        }

        Ok(())
    }

    fn navigate_to_parent<'a>(doc: &'a mut Value, segments: &[String]) -> Result<&'a mut Value, String> {
        if segments.is_empty() {
            return Err("Cannot navigate to parent with empty segments".to_string());
        }

        let mut current = doc;
        for segment in &segments[..segments.len() - 1] {
            if current.is_array() {
                let arr = current.as_array_mut().expect("checked is_array");
                let index = parse_array_index(segment, arr.len(), false)?;
                current = arr.get_mut(index).ok_or_else(|| {
                    format!("ChatGPT Web delta path does not exist: {}", segment)
                })?;
            } else if current.is_object() {
                let obj = current.as_object_mut().expect("checked is_object");
                current = obj.get_mut(segment).ok_or_else(|| {
                    format!("ChatGPT Web delta path does not exist: {}", segment)
                })?;
            } else {
                return Err("Delta path leads through non-container".to_string());
            }
        }
        Ok(current)
    }

    fn apply_at_root(&mut self, operation: &str, value: Option<Value>) -> Result<(), String> {
        match operation {
            "add" | "replace" => {
                self.document = value;
                Ok(())
            }
            "append" => {
                self.document = Some(append_value(self.document.clone(), value)?);
                Ok(())
            }
            "patch" => {
                if let Some(Value::Array(patches)) = value {
                    for patch in patches {
                        if !patch.is_object() {
                            return Err("ChatGPT Web patch entry must be an object".to_string());
                        }
                        let patch_obj = patch.as_object().unwrap();
                        let patch_path = patch_obj
                            .get("p")
                            .and_then(|v| v.as_str())
                            .ok_or("ChatGPT Web patch entry requires path")?;
                        let patch_op = patch_obj
                            .get("o")
                            .and_then(|v| v.as_str())
                            .ok_or("ChatGPT Web patch entry requires operation")?;
                        let patch_val = patch_obj.get("v").cloned();
                        self.apply_at(patch_path, patch_op, patch_val)?;
                    }
                    Ok(())
                } else {
                    Err("ChatGPT Web patch value must be an array".to_string())
                }
            }
            _ => Err(format!("Unsupported operation: {}", operation)),
        }
    }

    fn apply_to_array(
        target: &mut Vec<Value>,
        key: &str,
        operation: &str,
        value: Option<Value>,
    ) -> Result<(), String> {
        match operation {
            "patch" => Err("Nested ChatGPT Web patch is unsupported".to_string()),
            "add" => {
                let index = parse_array_index(key, target.len(), true)?;
                target.insert(index, value.unwrap_or(Value::Null));
                Ok(())
            }
            "append" => {
                let index = parse_array_index(key, target.len(), false)?;
                target[index] = append_value(Some(target[index].clone()), value)?;
                Ok(())
            }
            "replace" => {
                let index = parse_array_index(key, target.len(), false)?;
                target[index] = value.unwrap_or(Value::Null);
                Ok(())
            }
            _ => Err(format!("Unsupported operation: {}", operation)),
        }
    }

    fn apply_to_object(
        target: &mut serde_json::Map<String, Value>,
        key: &str,
        operation: &str,
        value: Option<Value>,
    ) -> Result<(), String> {
        match operation {
            "patch" => Err("Nested ChatGPT Web patch is unsupported".to_string()),
            "add" | "replace" => {
                target.insert(key.to_string(), value.unwrap_or(Value::Null));
                Ok(())
            }
            "append" => {
                let current = target.get(key).cloned();
                target.insert(key.to_string(), append_value(current, value)?);
                Ok(())
            }
            _ => Err(format!("Unsupported operation: {}", operation)),
        }
    }
}

fn assert_safe_value(value: &Value) -> Result<(), String> {
    match value {
        Value::Array(arr) => {
            for item in arr {
                assert_safe_value(item)?;
            }
            Ok(())
        }
        Value::Object(obj) => {
            for (key, val) in obj {
                if UNSAFE_POINTER_SEGMENTS.contains(&key.as_str()) {
                    return Err(format!("Unsafe object key in ChatGPT Web delta: {}", key));
                }
                assert_safe_value(val)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn append_value(current: Option<Value>, incoming: Option<Value>) -> Result<Value, String> {
    let incoming = match incoming {
        Some(v) => v,
        None => return Ok(current.unwrap_or(Value::Null)),
    };

    assert_safe_value(&incoming)?;

    match current {
        None => Ok(incoming),
        Some(Value::String(mut s)) if incoming.is_string() => {
            s.push_str(incoming.as_str().unwrap());
            Ok(Value::String(s))
        }
        Some(Value::Array(mut arr)) => {
            if let Value::Array(incoming_arr) = incoming {
                arr.extend(incoming_arr);
            } else {
                arr.push(incoming);
            }
            Ok(Value::Array(arr))
        }
        Some(Value::Object(mut obj)) if incoming.is_object() => {
            for (k, v) in incoming.as_object().unwrap() {
                obj.insert(k.clone(), v.clone());
            }
            Ok(Value::Object(obj))
        }
        Some(curr) => Err(format!(
            "Cannot append ChatGPT Web delta values ({}, {})",
            curr.type_str(),
            incoming.type_str()
        )),
    }
}

fn pointer_segments(pointer: &str) -> Result<Vec<String>, String> {
    if pointer.is_empty() {
        return Ok(vec![]);
    }
    if !pointer.starts_with('/') {
        return Err(format!("Invalid ChatGPT Web JSON Pointer: {}", pointer));
    }

    let segments = pointer[1..]
        .split('/')
        .map(|seg| {
            let decoded = seg.replace("~1", "/").replace("~0", "~");
            if UNSAFE_POINTER_SEGMENTS.contains(&decoded.as_str()) {
                Err(format!("Unsafe JSON Pointer segment: {}", decoded))
            } else {
                Ok(decoded)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(segments)
}

fn parse_array_index(segment: &str, length: usize, allow_end: bool) -> Result<usize, String> {
    if allow_end && segment == "-" {
        return Ok(length);
    }
    let index: usize = segment
        .parse()
        .map_err(|_| format!("Invalid ChatGPT Web array index: {}", segment))?;
    let upper_bound = if allow_end { length } else { length.saturating_sub(1) };
    if index > upper_bound {
        return Err(format!("ChatGPT Web array index is out of bounds: {}", segment));
    }
    Ok(index)
}

fn parse_chatgpt_web_encoded_item(encoded_item: &str) -> Result<Vec<SseEvent>, String> {
    let mut events = vec![];
    let mut event_name = "message".to_string();
    let mut data_lines = vec![];

    let dispatch = |events: &mut Vec<SseEvent>, event_name: &str, data_lines: &[String]| {
        if data_lines.is_empty() {
            return;
        }
        let data = data_lines.join("\n");
        let is_done = data == "[DONE]";
        let json = if data == "[DONE]" {
            None
        } else {
            serde_json::from_str::<Value>(&data).ok()
        };

        events.push(SseEvent {
            event: event_name.to_string(),
            data,
            json,
            done: is_done,
        });
    };

    for line in encoded_item.lines() {
        if line.is_empty() {
            dispatch(&mut events, &event_name, &data_lines);
            event_name = "message".to_string();
            data_lines.clear();
            continue;
        }
        if line.starts_with(':') {
            continue;
        }

        let (field, value) = if let Some(colon_pos) = line.find(':') {
            let field = &line[..colon_pos];
            let value = &line[colon_pos + 1..];
            let value = if value.starts_with(' ') { &value[1..] } else { value };
            (field, value)
        } else {
            (line, "")
        };

        if field == "event" {
            event_name = if value.is_empty() {
                "message".to_string()
            } else {
                value.to_string()
            };
        } else if field == "data" {
            data_lines.push(value.to_string());
        }
    }
    dispatch(&mut events, &event_name, &data_lines);

    Ok(events)
}

trait ValueTypeStr {
    fn type_str(&self) -> &'static str;
}

impl ValueTypeStr for Value {
    fn type_str(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

impl Default for ChatGptWebDeltaV1Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_encoded_item_simple() {
        let item = "event: message\ndata: {\"test\": \"value\"}\n\n";
        let events = parse_chatgpt_web_encoded_item(item).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
    }

    #[test]
    fn test_pointer_segments() {
        let segs = pointer_segments("/a/b/c").unwrap();
        assert_eq!(segs, vec!["a", "b", "c"]);

        let segs = pointer_segments("").unwrap();
        assert_eq!(segs.len(), 0);

        let err = pointer_segments("no-slash");
        assert!(err.is_err());
    }

    #[test]
    fn test_pointer_unescaping() {
        let segs = pointer_segments("/a~1b/c~0d").unwrap();
        assert_eq!(segs[0], "a/b");
        assert_eq!(segs[1], "c~d");
    }

    #[test]
    fn test_decoder_basic() {
        let mut decoder = ChatGptWebDeltaV1Decoder::new();
        let item = "event: delta_encoding\ndata: \"v1\"\n\n";
        let result = decoder.ingest(item).unwrap();
        assert!(!result.changed);
        assert!(!result.done);
    }

    #[test]
    fn test_unsafe_keys_rejected() {
        let err = assert_safe_value(&json!({"__proto__": "bad"}));
        assert!(err.is_err());
    }
}
