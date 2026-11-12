//! Game values to and from the JSON plugins see. UUIDs are hyphenated strings, block states their
//! numeric ids, text is a string (`§` colour codes allowed), a player is an object.
use horizon_client::mod_api::{BlockStateId, NativeServerPlayer, Text};
use serde_json::{Value, json};

pub trait ToJson {
    fn to_json(&self) -> Value;
}

pub trait FromJson: Sized {
    fn from_json(value: &Value) -> Result<Self, String>;
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            fn to_json(&self) -> Value { json!(*self) }
        }
        impl FromJson for $t {
            fn from_json(value: &Value) -> Result<Self, String> {
                if let Some(n) = value.as_i64() {
                    return <$t>::try_from(n).map_err(|_| format!("{n} is out of range"));
                }
                if let Some(n) = value.as_u64() {
                    return <$t>::try_from(n).map_err(|_| format!("{n} is out of range"));
                }
                Err(format!("expected an integer, got {value}"))
            }
        }
    )*};
}
numbers!(i8, i16, i32, i64, u8, u16, u32, u64, usize);

macro_rules! floats {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            fn to_json(&self) -> Value { json!(*self) }
        }
        impl FromJson for $t {
            fn from_json(value: &Value) -> Result<Self, String> {
                value.as_f64().map(|v| v as $t).ok_or_else(|| format!("expected a number, got {value}"))
            }
        }
    )*};
}
floats!(f32, f64);

impl ToJson for bool {
    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }
}
impl FromJson for bool {
    fn from_json(value: &Value) -> Result<Self, String> {
        value
            .as_bool()
            .ok_or_else(|| format!("expected true or false, got {value}"))
    }
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
pub fn uuid_string(uuid: u128) -> String {
    let h = format!("{uuid:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

pub fn parse_uuid(text: &str) -> Option<u128> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    (hex.len() == 32)
        .then(|| u128::from_str_radix(&hex, 16).ok())
        .flatten()
}

impl ToJson for u128 {
    fn to_json(&self) -> Value {
        Value::String(uuid_string(*self))
    }
}
impl FromJson for u128 {
    fn from_json(value: &Value) -> Result<Self, String> {
        match value {
            Value::String(s) => parse_uuid(s).ok_or_else(|| format!("`{s}` is not a UUID")),
            Value::Number(n) => n
                .as_u64()
                .map(u128::from)
                .ok_or_else(|| format!("`{n}` is not a UUID")),
            other => Err(format!("expected a UUID string, got {other}")),
        }
    }
}

impl ToJson for str {
    fn to_json(&self) -> Value {
        Value::String(self.to_owned())
    }
}
impl ToJson for String {
    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }
}
impl FromJson for String {
    fn from_json(value: &Value) -> Result<Self, String> {
        match value {
            Value::String(s) => Ok(s.clone()),
            other => Err(format!("expected a string, got {other}")),
        }
    }
}

impl ToJson for Text {
    fn to_json(&self) -> Value {
        Value::String(self.plain())
    }
}
impl FromJson for Text {
    fn from_json(value: &Value) -> Result<Self, String> {
        match value {
            Value::String(s) if s.contains('§') => Ok(Text::legacy(s)),
            Value::String(s) => Ok(Text::literal(s.clone())),
            other => Ok(Text::literal(other.to_string())),
        }
    }
}

impl ToJson for BlockStateId {
    fn to_json(&self) -> Value {
        json!(self.0)
    }
}
impl FromJson for BlockStateId {
    fn from_json(value: &Value) -> Result<Self, String> {
        u32::from_json(value).map(BlockStateId)
    }
}

impl ToJson for NativeServerPlayer {
    fn to_json(&self) -> Value {
        json!({
            "uuid": uuid_string(self.uuid),
            "name": self.name,
            "permission": self.permission,
            "dimension": self.dimension,
            "position": self.position,
            "game_mode": self.game_mode,
            "health": self.health,
            "yaw": self.yaw,
            "pitch": self.pitch,
        })
    }
}

impl ToJson for () {
    fn to_json(&self) -> Value {
        Value::Null
    }
}

impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self) -> Value {
        self.as_ref().map_or(Value::Null, T::to_json)
    }
}
impl<T: FromJson> FromJson for Option<T> {
    fn from_json(value: &Value) -> Result<Self, String> {
        if value.is_null() {
            Ok(None)
        } else {
            T::from_json(value).map(Some)
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(T::to_json).collect())
    }
}
impl<T: ToJson> ToJson for Vec<T> {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}
impl<T: FromJson> FromJson for Vec<T> {
    fn from_json(value: &Value) -> Result<Self, String> {
        value
            .as_array()
            .ok_or_else(|| format!("expected a list, got {value}"))?
            .iter()
            .map(T::from_json)
            .collect()
    }
}

impl<T: ToJson, const N: usize> ToJson for [T; N] {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}
impl<T: FromJson, const N: usize> FromJson for [T; N] {
    fn from_json(value: &Value) -> Result<Self, String> {
        let items = Vec::<T>::from_json(value)?;
        let len = items.len();
        items
            .try_into()
            .map_err(|_| format!("expected {N} values, got {len}"))
    }
}

impl<T: ToJson, E: ToJson> ToJson for Result<T, E> {
    fn to_json(&self) -> Value {
        match self {
            Ok(v) => json!({ "ok": v.to_json() }),
            Err(e) => json!({ "err": e.to_json() }),
        }
    }
}

macro_rules! tuples {
    ($(($($n:tt $t:ident),+)),*) => {$(
        impl<$($t: ToJson),+> ToJson for ($($t,)+) {
            fn to_json(&self) -> Value { Value::Array(vec![$(self.$n.to_json()),+]) }
        }
        impl<$($t: FromJson),+> FromJson for ($($t,)+) {
            fn from_json(value: &Value) -> Result<Self, String> {
                let items = value.as_array().ok_or_else(|| format!("expected a list, got {value}"))?;
                Ok(($($t::from_json(items.get($n).unwrap_or(&Value::Null))?,)+))
            }
        }
    )*};
}
tuples!((0 A), (0 A, 1 B), (0 A, 1 B, 2 C));

/// Argument `index` of a host call.
pub fn arg<T: FromJson>(args: &[Value], index: usize) -> Result<T, String> {
    T::from_json(args.get(index).unwrap_or(&Value::Null))
        .map_err(|e| format!("argument {}: {e}", index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_round_trip() {
        let u = 0x069a79f4_44e9_4726_a5be_fca90e38aaf5u128;
        assert_eq!(uuid_string(u), "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(u128::from_json(&u.to_json()).unwrap(), u);
    }

    #[test]
    fn arrays_and_options() {
        let v = [1i32, -2, 3].to_json();
        assert_eq!(<[i32; 3]>::from_json(&v).unwrap(), [1, -2, 3]);
        assert!(<[i32; 2]>::from_json(&v).is_err());
        assert_eq!(Option::<u8>::from_json(&Value::Null).unwrap(), None);
        assert!(u8::from_json(&json!(300)).is_err());
    }
}
