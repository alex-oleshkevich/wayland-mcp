//! Typed D-Bus method calls. The caller supplies destination/path/interface/method,
//! an explicit signature and JSON args converted per that signature. Invalid
//! signatures/args are rejected before any call is emitted. This is a general
//! structured call: it retains the caller's normal bus permissions and is not
//! restricted to read-only methods.
use zbus::zvariant::{self, OwnedValue, Structure, Value};

/// Split a D-Bus signature into top-level type strings.
pub fn split_signature(sig: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = sig.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        i = consume_type(&chars, i)?;
        out.push(chars[start..i].iter().collect());
    }
    Ok(out)
}
fn consume_type(chars: &[char], mut i: usize) -> Result<usize, String> {
    match chars.get(i) {
        None => Err("unexpected end of signature".into()),
        Some('a') => {
            i += 1;
            if chars.get(i) == Some(&'{') {
                i += 1;
                i = consume_type(chars, i)?; // key (basic)
                i = consume_type(chars, i)?; // value
                if chars.get(i) != Some(&'}') { return Err("unterminated dict entry in signature".into()); }
                Ok(i + 1)
            } else { consume_type(chars, i) }
        }
        Some('(') => {
            i += 1;
            while chars.get(i) != Some(&')') {
                if i >= chars.len() { return Err("unterminated struct in signature".into()); }
                i = consume_type(chars, i)?;
            }
            Ok(i + 1)
        }
        Some(c) if "ybnqiuxthdsogv".contains(*c) => Ok(i + 1),
        Some(c) => Err(format!("invalid character '{c}' in signature")),
    }
}

pub fn json_to_value(sig: &str, json: &serde_json::Value) -> Result<Value<'static>, String> {
    let v = match sig {
        "y" => Value::U8(json.as_u64().and_then(|n| u8::try_from(n).ok()).ok_or("expected u8 number")?),
        "b" => Value::Bool(json.as_bool().ok_or("expected boolean")?),
        "n" => Value::I16(json.as_i64().and_then(|n| i16::try_from(n).ok()).ok_or("expected i16")?),
        "q" => Value::U16(json.as_u64().and_then(|n| u16::try_from(n).ok()).ok_or("expected u16")?),
        "i" => Value::I32(json.as_i64().and_then(|n| i32::try_from(n).ok()).ok_or("expected i32")?),
        "u" => Value::U32(json.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or("expected u32")?),
        "x" => Value::I64(json.as_i64().ok_or("expected i64")?),
        "t" => Value::U64(json.as_u64().ok_or("expected u64")?),
        "d" => Value::F64(json.as_f64().ok_or("expected number")?),
        "s" => Value::Str(json.as_str().ok_or("expected string")?.to_string().into()),
        "o" => Value::ObjectPath(zvariant::ObjectPath::try_from(json.as_str().ok_or("expected object path string")?).map_err(|e| e.to_string())?.to_owned()),
        "g" => Value::Signature(json.as_str().ok_or("expected signature string")?.parse::<zvariant::Signature>().map_err(|e: zvariant::signature::Error| e.to_string())?),
        "v" => {
            // Infer from JSON.
            match json {
                serde_json::Value::String(s) => Value::Value(Box::new(Value::Str(s.clone().into()))),
                serde_json::Value::Bool(b) => Value::Value(Box::new(Value::Bool(*b))),
                serde_json::Value::Number(n) => Value::Value(Box::new(if let Some(u) = n.as_u64() { Value::U64(u) } else if let Some(i) = n.as_i64() { Value::I64(i) } else { Value::F64(n.as_f64().unwrap()) })),
                _ => return Err("variant args need a string, boolean or number JSON value in this tool".into()),
            }
        }
        _ if sig.starts_with('a') && !sig.starts_with("a{") => {
            let elem = &sig[1..];
            let arr = json.as_array().ok_or("expected JSON array")?;
            let mut values = Vec::new();
            for item in arr { values.push(json_to_value(elem, item)?); }
            // Homogeneous typed vectors for common element types.
            // Fallback: construct via homogeneous typed vectors for common element types.
            match elem {
                "s" => Value::new(arr.iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect::<Vec<String>>()),
                "u" => Value::new(arr.iter().map(|v| v.as_u64().unwrap_or(0) as u32).collect::<Vec<u32>>()),
                "i" => Value::new(arr.iter().map(|v| v.as_i64().unwrap_or(0) as i32).collect::<Vec<i32>>()),
                "b" => Value::new(arr.iter().map(|v| v.as_bool().unwrap_or(false)).collect::<Vec<bool>>()),
                _ => { let _ = values; return Err(format!("array element type '{elem}' not supported by dbus_call args (supported: basic scalars, as/au/ai/ab, a{{sv}})")); }
            }
        }
        "a{sv}" => {
            let obj = json.as_object().ok_or("expected JSON object for a{sv}")?;
            let mut map = std::collections::HashMap::new();
            for (k, v) in obj { map.insert(k.clone(), json_to_value("v", v)?); }
            Value::new(map)
        }
        _ if sig.starts_with('(') && sig.ends_with(')') => {
            let inner = split_signature(&sig[1..sig.len() - 1])?;
            let arr = json.as_array().ok_or("expected JSON array for struct arg")?;
            if arr.len() != inner.len() { return Err("struct arg arity mismatch".into()); }
            let mut fields = Vec::new();
            for (s, v) in inner.iter().zip(arr.iter()) { fields.push(json_to_value(s, v)?); }
            Value::Structure({ let mut b = zvariant::StructureBuilder::new(); for f in fields { b = b.append_field(f); } b.build().map_err(|e| e.to_string())? })
        }
        other => return Err(format!("unsupported signature type '{other}' for dbus_call args")),
    };
    Ok(v)
}

pub fn value_to_json(v: &OwnedValue) -> serde_json::Value {
    match &**v {
        Value::Bool(b) => serde_json::json!(b),
        Value::U8(n) => serde_json::json!(n),
        Value::I16(n) => serde_json::json!(n),
        Value::U16(n) => serde_json::json!(n),
        Value::I32(n) => serde_json::json!(n),
        Value::U32(n) => serde_json::json!(n),
        Value::I64(n) => serde_json::json!(n),
        Value::U64(n) => serde_json::json!(n),
        Value::F64(n) => serde_json::json!(n),
        Value::Str(s) => serde_json::json!(s.as_str()),
        Value::ObjectPath(p) => serde_json::json!(p.as_str()),
        Value::Signature(s) => serde_json::json!(s.to_string()),
        Value::Value(inner) => match OwnedValue::try_from((**inner).clone()) { Ok(ov) => value_to_json(&ov), Err(_) => serde_json::json!(format!("{inner:?}")) },
        Value::Array(a) => {
            let mut out = Vec::new();
            for item in a.iter() { if let Ok(ov) = OwnedValue::try_from(item.clone()) { out.push(value_to_json(&ov)); } }
            serde_json::Value::Array(out)
        }
        Value::Dict(d) => {
            let mut map = serde_json::Map::new();
            for (k, val) in d.iter() {
                let key = match k { Value::Str(s) => s.as_str().to_string(), other => format!("{other:?}") };
                if let Ok(ov) = OwnedValue::try_from(val.clone()) { map.insert(key, value_to_json(&ov)); }
            }
            serde_json::Value::Object(map)
        }
        Value::Structure(s) => {
            let fields: Vec<serde_json::Value> = s.fields().iter().filter_map(|f| OwnedValue::try_from(f.clone()).ok().map(|ov| value_to_json(&ov))).collect();
            serde_json::Value::Array(fields)
        }
        other => serde_json::json!(format!("{other:?}")),
    }
}

pub async fn call(conn: &zbus::Connection, destination: &str, path: &str, interface: &str, method: &str, signature: &str, args: &[serde_json::Value]) -> Result<serde_json::Value, String> {
    let types = split_signature(signature)?;
    if types.len() != args.len() { return Err(format!("signature '{signature}' has {} argument(s) but {} were supplied; no call emitted", types.len(), args.len())); }
    let mut fields = Vec::new();
    for (t, a) in types.iter().zip(args.iter()) { fields.push(json_to_value(t, a)?); }
    let reply = if fields.is_empty() {
        conn.call_method(Some(destination), path, Some(interface), method, &()).await
    } else {
        let mut b = zvariant::StructureBuilder::new();
        for f in fields { b = b.append_field(f); }
        let body = b.build().map_err(|e| e.to_string())?;
        conn.call_method(Some(destination), path, Some(interface), method, &body).await
    }.map_err(|e| format!("D-Bus error calling {interface}.{method}: {e}"))?;
    // Decode the reply dynamically as a structure over its own signature.
    match reply.body().deserialize::<Structure>() {
        Ok(structure) => {
            let fields: Vec<serde_json::Value> = structure.fields().iter().filter_map(|f| OwnedValue::try_from(f.clone()).ok().map(|ov| value_to_json(&ov))).collect();
            Ok(serde_json::json!({"destination": destination, "interface": interface, "method": method, "reply": fields}))
        }
        Err(struct_err) => match reply.body().deserialize::<OwnedValue>() {
            Ok(v) => Ok(serde_json::json!({"destination": destination, "interface": interface, "method": method, "reply": value_to_json(&v)})),
            Err(_) => Ok(serde_json::json!({"destination": destination, "interface": interface, "method": method, "reply": null, "note": format!("reply could not be decoded generically ({struct_err}); the call itself succeeded")})),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn split_sigs() {
        assert_eq!(split_signature("sa{sv}(iu)").unwrap(), vec!["s", "a{sv}", "(iu)"]);
        assert!(split_signature("sz").is_err());
    }
    #[test] fn arity_checked() { assert_eq!(split_signature("su").unwrap().len(), 2); }
}
