use std::borrow::Cow;
use std::collections::HashMap;

use qdrant_client::qdrant::{
    point_id::PointIdOptions, vectors::VectorsOptions, PointId, ScoredPoint, Value, Vectors,
};

use super::schema::Payload;

macro_rules! val_str(($hash:ident, $val:expr) => { serde_json::from_value($hash.remove($val).unwrap()).unwrap() });
macro_rules! val_parse_str(($hash:ident, $val:expr) => {
    serde_json::from_value::<Cow<'_, str>>($hash.remove($val).unwrap())
        .unwrap()
        .parse()
        .unwrap()
});

impl Payload {
    pub fn into_qdrant(self) -> HashMap<String, qdrant_client::qdrant::Value> {
        HashMap::from([
            ("lang".into(), self.lang.to_ascii_lowercase().into()),
            ("repo_name".into(), self.repo_name.into()),
            ("repo_ref".into(), self.repo_ref.into()),
            ("relative_path".into(), self.relative_path.into()),
            ("content_hash".into(), self.content_hash.into()),
            ("snippet".into(), self.text.into()),
            ("start_line".into(), self.start_line.to_string().into()),
            ("end_line".into(), self.end_line.to_string().into()),
            ("start_byte".into(), self.start_byte.to_string().into()),
            ("end_byte".into(), self.end_byte.to_string().into()),
            ("branches".into(), self.branches.into()),
            ("commit_hash".into(), self.commit_hash.into()),
        ])
    }

    pub fn from_qdrant(orig: ScoredPoint) -> Payload {
        let ScoredPoint {
            id,
            payload,
            score,
            vectors,
            ..
        } = orig;

        parse_payload(id, vectors, payload, score)
    }
}

fn parse_payload(
    id: Option<PointId>,
    vectors: Option<Vectors>,
    payload: HashMap<String, Value>,
    score: f32,
) -> Payload {
    let Some(PointId {
        point_id_options: Some(PointIdOptions::Uuid(id)),
    }) = id
    else {
        // unless the db was corrupted/written by someone else,
        // this shouldn't happen
        unreachable!("corrupted db");
    };

    let embedding = match vectors {
        None => None,
        Some(Vectors {
            vectors_options: Some(VectorsOptions::Vector(v)),
        }) => Some(v.data),
        _ => {
            // this also should probably never happen
            unreachable!("got non-vector value");
        }
    };

    let mut converted = payload
        .into_iter()
        .map(|(key, value)| (key, kind_to_value(value.kind)))
        .collect::<HashMap<String, serde_json::Value>>();

    Payload {
        lang: val_str!(converted, "lang"),
        repo_name: val_str!(converted, "repo_name"),
        repo_ref: val_str!(converted, "repo_ref"),
        relative_path: val_str!(converted, "relative_path"),
        content_hash: val_str!(converted, "content_hash"),
        text: val_str!(converted, "snippet"),
        branches: val_str!(converted, "branches"),
        start_line: val_parse_str!(converted, "start_line"),
        end_line: val_parse_str!(converted, "end_line"),
        start_byte: val_parse_str!(converted, "start_byte"),
        end_byte: val_parse_str!(converted, "end_byte"),
        commit_hash: val_str!(converted, "commit_hash"),

        id: Some(id),
        score: Some(score),
        embedding,
    }
}

fn kind_to_value(kind: Option<qdrant_client::qdrant::value::Kind>) -> serde_json::Value {
    use qdrant_client::qdrant::value::Kind;
    match kind {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(v)) => serde_json::Value::Bool(v),
        Some(Kind::DoubleValue(v)) => {
            serde_json::Value::Number(serde_json::Number::from_f64(v).unwrap())
        }
        Some(Kind::IntegerValue(v)) => serde_json::Value::Number(v.into()),
        Some(Kind::StringValue(v)) => serde_json::Value::String(v),
        Some(Kind::ListValue(v)) => serde_json::Value::Array(
            v.values
                .into_iter()
                .map(|v| kind_to_value(v.kind))
                .collect(),
        ),
        Some(Kind::StructValue(_v)) => todo!(),
        None => serde_json::Value::Null,
    }
}
