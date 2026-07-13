//! PBLite encoder/decoder.
//!
//! PBLite is Google's array-of-arrays JSON form for protobuf messages. Each
//! protobuf message becomes a JSON array indexed by `field_number - 1`:
//!
//! - `int32`/`int64`/`uint*` → JSON number (or string for >2^53)
//! - `string` → JSON string
//! - `bytes` → base64 string (standard, with padding)
//! - `bool` → JSON bool (or number)
//! - `repeated T` → JSON array of T
//! - `message T` → recursively-encoded JSON array
//!
//! Fields annotated `[(pblite.pblite_binary) = true]` are encoded as base64
//! strings even though the underlying field is `string`/`bytes`/`message` —
//! Google's way of smuggling binary blobs through the JSON form.
//!
//! Mirrors the semantics of `go.mau.fi/util/pblite`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use prost::Message as _;
use prost_reflect::{DynamicMessage, FieldDescriptor, Kind, ReflectMessage, Value};
use serde_json::Value as Json;

use crate::{Error, Result};

/// Marshal a prost message as PBLite JSON bytes.
pub fn marshal<M: ReflectMessage + prost::Message>(msg: &M) -> Result<Vec<u8>> {
    let dyn_msg = msg_to_dynamic(msg)?;
    let arr = serialize_message(&dyn_msg)?;
    serde_json::to_vec(&arr).map_err(Error::from)
}

/// Unmarshal PBLite JSON bytes into a prost message.
pub fn unmarshal<M: ReflectMessage + prost::Message + Default>(bytes: &[u8]) -> Result<M> {
    let json: Json = serde_json::from_slice(bytes)?;
    let arr = match json {
        Json::Array(a) => a,
        other => {
            return Err(Error::PBLite(format!(
                "expected JSON array at top level, got {}",
                json_type_name(&other)
            )));
        }
    };

    let descriptor = M::default().descriptor();
    let mut dyn_msg = DynamicMessage::new(descriptor);
    deserialize_into(&arr, &mut dyn_msg)?;

    // Round-trip via protobuf wire format to convert DynamicMessage → M.
    let buf = dyn_msg.encode_to_vec();
    M::decode(&*buf).map_err(Error::from)
}

fn msg_to_dynamic<M: ReflectMessage + prost::Message>(msg: &M) -> Result<DynamicMessage> {
    let buf = msg.encode_to_vec();
    DynamicMessage::decode(msg.descriptor(), &*buf)
        .map_err(|e| Error::PBLite(format!("transcode prost->dynamic: {e}")))
}

// ---- serialization (message → array) ----

fn serialize_message(msg: &DynamicMessage) -> Result<Vec<Json>> {
    let descriptor = msg.descriptor();
    let max_field = descriptor.fields().map(|f| f.number()).max().unwrap_or(0) as usize;
    let mut out = vec![Json::Null; max_field];

    for field in descriptor.fields() {
        if !msg.has_field(&field) {
            continue;
        }
        let value = msg.get_field(&field);
        let json_val = serialize_value(&field, &value)?;
        out[(field.number() - 1) as usize] = json_val;
    }

    Ok(out)
}

fn serialize_value(field: &FieldDescriptor, value: &Value) -> Result<Json> {
    if field.is_list()
        && let Value::List(list) = value
    {
        let mut arr = Vec::with_capacity(list.len());
        for item in list {
            arr.push(serialize_scalar(field, item)?);
        }
        return Ok(Json::Array(arr));
    }
    serialize_scalar(field, value)
}

fn serialize_scalar(field: &FieldDescriptor, value: &Value) -> Result<Json> {
    let pblite_binary = is_pblite_binary(field);
    let kind = field.kind();
    Ok(match (&kind, value) {
        (Kind::Message(_), Value::Message(m)) => {
            if pblite_binary {
                let bytes = m.encode_to_vec();
                Json::String(B64.encode(bytes))
            } else {
                Json::Array(serialize_message(m)?)
            }
        }
        (Kind::Bytes, Value::Bytes(b)) => Json::String(B64.encode(b.as_ref())),
        (Kind::String, Value::String(s)) => {
            if pblite_binary {
                Json::String(B64.encode(s.as_bytes()))
            } else {
                Json::String(s.clone())
            }
        }
        (Kind::Bool, Value::Bool(b)) => Json::Bool(*b),
        (Kind::Int32 | Kind::Sint32 | Kind::Sfixed32, Value::I32(n)) => Json::Number((*n).into()),
        (Kind::Int64 | Kind::Sint64 | Kind::Sfixed64, Value::I64(n)) => Json::Number((*n).into()),
        (Kind::Uint32 | Kind::Fixed32, Value::U32(n)) => Json::Number((*n).into()),
        (Kind::Uint64 | Kind::Fixed64, Value::U64(n)) => Json::Number((*n).into()),
        (Kind::Float, Value::F32(n)) => serde_json::Number::from_f64(*n as f64)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        (Kind::Double, Value::F64(n)) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        (Kind::Enum(_), Value::EnumNumber(n)) => Json::Number((*n).into()),
        (k, _) => {
            return Err(Error::PBLite(format!(
                "unsupported field {} (kind={:?})",
                field.full_name(),
                k,
            )));
        }
    })
}

// ---- deserialization (array → message) ----

fn deserialize_into(data: &[Json], msg: &mut DynamicMessage) -> Result<()> {
    let descriptor = msg.descriptor();
    for field in descriptor.fields() {
        let idx = (field.number() - 1) as usize;
        if idx >= data.len() {
            continue;
        }
        let raw = &data[idx];
        if matches!(raw, Json::Null) {
            continue;
        }
        let val = deserialize_value(&field, raw)?;
        msg.set_field(&field, val);
    }
    Ok(())
}

fn deserialize_value(field: &FieldDescriptor, raw: &Json) -> Result<Value> {
    if field.is_list() {
        let arr = match raw {
            Json::Array(a) => a,
            other => {
                return Err(Error::PBLite(format!(
                    "expected array for repeated field {}, got {}",
                    field.full_name(),
                    json_type_name(other)
                )));
            }
        };
        let mut list = Vec::with_capacity(arr.len());
        for item in arr {
            list.push(deserialize_scalar(field, item)?);
        }
        return Ok(Value::List(list));
    }
    deserialize_scalar(field, raw)
}

fn deserialize_scalar(field: &FieldDescriptor, raw: &Json) -> Result<Value> {
    let pblite_binary = is_pblite_binary(field);
    Ok(match field.kind() {
        Kind::Message(message_desc) => {
            if pblite_binary {
                let s = expect_string(raw, field)?;
                let bytes = B64.decode(s).map_err(Error::from)?;
                let msg = DynamicMessage::decode(message_desc, &*bytes)
                    .map_err(|e| Error::PBLite(format!("decode binary submsg: {e}")))?;
                Value::Message(msg)
            } else {
                let arr = match raw {
                    Json::Array(a) => a,
                    other => {
                        return Err(Error::PBLite(format!(
                            "expected array for message field {}, got {}",
                            field.full_name(),
                            json_type_name(other)
                        )));
                    }
                };
                let mut nested = DynamicMessage::new(message_desc);
                deserialize_into(arr, &mut nested)?;
                Value::Message(nested)
            }
        }
        Kind::Bytes => {
            let s = expect_string(raw, field)?;
            Value::Bytes(B64.decode(s).map_err(Error::from)?.into())
        }
        Kind::String => {
            let s = expect_string(raw, field)?;
            if pblite_binary {
                let bytes = B64.decode(s).map_err(Error::from)?;
                Value::String(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                Value::String(s.to_string())
            }
        }
        Kind::Bool => match raw {
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Bool(n.as_f64().unwrap_or(0.0) != 0.0),
            other => {
                return Err(Error::PBLite(format!(
                    "expected bool/number for field {}, got {}",
                    field.full_name(),
                    json_type_name(other)
                )));
            }
        },
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(expect_i64(raw, field)? as i32),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(expect_i64(raw, field)?),
        Kind::Uint32 | Kind::Fixed32 => Value::U32(expect_u64(raw, field)? as u32),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(expect_u64(raw, field)?),
        Kind::Float => Value::F32(expect_f64(raw, field)? as f32),
        Kind::Double => Value::F64(expect_f64(raw, field)?),
        Kind::Enum(_) => Value::EnumNumber(expect_i64(raw, field)? as i32),
    })
}

fn expect_string<'a>(raw: &'a Json, field: &FieldDescriptor) -> Result<&'a str> {
    match raw {
        Json::String(s) => Ok(s.as_str()),
        other => Err(Error::PBLite(format!(
            "expected string for field {}, got {}",
            field.full_name(),
            json_type_name(other)
        ))),
    }
}

fn expect_i64(raw: &Json, field: &FieldDescriptor) -> Result<i64> {
    match raw {
        Json::Number(n) => n.as_i64().ok_or_else(|| {
            Error::PBLite(format!(
                "value out of i64 range for field {}",
                field.full_name()
            ))
        }),
        Json::String(s) => s
            .parse::<i64>()
            .map_err(|e| Error::PBLite(format!("parse i64 for {}: {e}", field.full_name()))),
        other => Err(Error::PBLite(format!(
            "expected number/string for {}, got {}",
            field.full_name(),
            json_type_name(other)
        ))),
    }
}

fn expect_u64(raw: &Json, field: &FieldDescriptor) -> Result<u64> {
    match raw {
        Json::Number(n) => n.as_u64().ok_or_else(|| {
            Error::PBLite(format!(
                "value out of u64 range for field {}",
                field.full_name()
            ))
        }),
        Json::String(s) => s
            .parse::<u64>()
            .map_err(|e| Error::PBLite(format!("parse u64 for {}: {e}", field.full_name()))),
        other => Err(Error::PBLite(format!(
            "expected number/string for {}, got {}",
            field.full_name(),
            json_type_name(other)
        ))),
    }
}

fn expect_f64(raw: &Json, field: &FieldDescriptor) -> Result<f64> {
    match raw {
        Json::Number(n) => n.as_f64().ok_or_else(|| {
            Error::PBLite(format!(
                "value out of f64 range for field {}",
                field.full_name()
            ))
        }),
        other => Err(Error::PBLite(format!(
            "expected number for {}, got {}",
            field.full_name(),
            json_type_name(other)
        ))),
    }
}

fn json_type_name(j: &Json) -> &'static str {
    match j {
        Json::Null => "null",
        Json::Bool(_) => "bool",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

/// Look up the `pblite.pblite_binary` field option (extension #50000) on a
/// field. Returns true when the option is set.
fn is_pblite_binary(field: &FieldDescriptor) -> bool {
    let pool = field.parent_message().parent_pool();
    if let Some(ext) = pool.get_extension_by_name("pblite.pblite_binary") {
        let opts = field.options();
        if let Some(v) = opts.get_extension(&ext).as_bool() {
            return v;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmproto::authentication::{AuthMessage, ConfigVersion};

    #[test]
    fn roundtrip_simple_message() {
        let msg = AuthMessage {
            request_id: "abc-123".into(),
            network: "Bugle".into(),
            tachyon_auth_token: vec![1, 2, 3, 4],
            config_version: Some(ConfigVersion {
                year: 2026,
                month: 3,
                day: 18,
                v1: 4,
                v2: 6,
            }),
        };
        let bytes = marshal(&msg).expect("marshal");
        let s = std::str::from_utf8(&bytes).unwrap();
        // Field 1 = request_id, field 2 = network, field 3 = config, field 6 = token.
        // (Order in proto file decides positions, but we can at least confirm valid JSON.)
        assert!(s.starts_with('['));
        let back: AuthMessage = unmarshal(&bytes).expect("unmarshal");
        assert_eq!(back, msg);
    }
}
