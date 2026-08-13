//! Checked-in `prost` representation of the ONNX messages needed to validate
//! the top-level graph boundary.
//!
//! Field numbers and wire types are generated from ONNX `onnx.proto3` at tag
//! `v1.20.0` (SHA-256
//! `470e64dfc5338477d3adc1853f5875618f70cc698306d9bed1232305680b121f`).
//! The full source remains upstream; the release audit records its immutable
//! URL, digest, and Apache-2.0 license. A bounded wire preflight runs before
//! these messages are decoded, so `prost` never receives unbounded counts,
//! lengths, or nesting.
#![allow(
    clippy::enum_variant_names,
    reason = "prost preserves the official TypeProto oneof field names"
)]

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ModelProto {
    #[prost(int64, tag = "1")]
    pub ir_version: i64,
    #[prost(message, repeated, tag = "8")]
    pub opset_import: Vec<OperatorSetIdProto>,
    #[prost(message, optional, tag = "7")]
    pub graph: Option<GraphProto>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct OperatorSetIdProto {
    #[prost(string, tag = "1")]
    pub domain: String,
    #[prost(int64, tag = "2")]
    pub version: i64,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct GraphProto {
    #[prost(message, repeated, tag = "5")]
    pub initializer: Vec<TensorProto>,
    #[prost(message, repeated, tag = "15")]
    pub sparse_initializer: Vec<SparseTensorProto>,
    #[prost(message, repeated, tag = "11")]
    pub input: Vec<ValueInfoProto>,
    #[prost(message, repeated, tag = "12")]
    pub output: Vec<ValueInfoProto>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SparseTensorProto {
    #[prost(message, optional, tag = "1")]
    pub values: Option<TensorProto>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct TensorProto {
    #[prost(string, tag = "8")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ValueInfoProto {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub r#type: Option<TypeProto>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct TypeProto {
    #[prost(oneof = "type_proto::Value", tags = "1, 4, 5, 8, 9")]
    pub value: Option<type_proto::Value>,
}

pub(crate) mod type_proto {
    use prost::{Message, Oneof};

    #[derive(Clone, PartialEq, Oneof)]
    pub(crate) enum Value {
        #[prost(message, tag = "1")]
        TensorType(Tensor),
        #[prost(message, tag = "4")]
        SequenceType(Unsupported),
        #[prost(message, tag = "5")]
        MapType(Unsupported),
        #[prost(message, tag = "8")]
        SparseTensorType(Unsupported),
        #[prost(message, tag = "9")]
        OptionalType(Unsupported),
    }

    #[derive(Clone, PartialEq, Message)]
    pub(crate) struct Tensor {
        #[prost(int32, tag = "1")]
        pub elem_type: i32,
        #[prost(message, optional, tag = "2")]
        pub shape: Option<super::TensorShapeProto>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub(crate) struct Unsupported {}
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct TensorShapeProto {
    #[prost(message, repeated, tag = "1")]
    pub dim: Vec<tensor_shape_proto::Dimension>,
}

pub(crate) mod tensor_shape_proto {
    use prost::Message;

    #[derive(Clone, PartialEq, Message)]
    pub(crate) struct Dimension {
        #[prost(oneof = "dimension::Value", tags = "1, 2")]
        pub value: Option<dimension::Value>,
    }

    pub(crate) mod dimension {
        use prost::Oneof;

        #[derive(Clone, PartialEq, Oneof)]
        pub(crate) enum Value {
            #[prost(int64, tag = "1")]
            DimValue(i64),
            #[prost(string, tag = "2")]
            DimParam(String),
        }
    }
}
