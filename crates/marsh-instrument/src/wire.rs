//! Lossless serde adapters for the actual, unmodified upstream types.
//! Remote derives generate field visitors; these declarations are never runtime DTOs.

use lurk_cli::syscall_info::{RetCode, SyscallArg, SyscallArgs, SyscallInfo};
use nix::unistd::Pid;
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;
use syscalls::Sysno;

#[derive(Serialize)]
#[serde(remote = "SyscallInfo")]
pub(crate) struct Info {
    typ: &'static str,
    #[serde(serialize_with = "serialize_pid")]
    pid: Pid,
    syscall: Sysno,
    #[serde(with = "Arguments")]
    args: SyscallArgs,
    #[serde(with = "ResultCode")]
    result: RetCode,
    duration: Duration,
}

impl Info {
    // A remote Deserialize derive would require 'de: 'static for upstream's tag. Read the
    // fields into their native types instead, validating the tag without leaking a String.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SyscallInfo, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Typ,
            Pid,
            Syscall,
            Args,
            Result,
            Duration,
        }
        struct NativeInfo;
        impl<'de> Visitor<'de> for NativeInfo {
            type Value = SyscallInfo;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("native SyscallInfo fields")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let (mut typ, mut pid, mut syscall, mut args, mut result, mut duration) =
                    (None, None, None, None, None, None);
                while let Some(field) = map.next_key()? {
                    match field {
                        Field::Typ if typ.is_none() => typ = Some(map.next_value_seed(Tag)?),
                        Field::Pid if pid.is_none() => pid = Some(Pid::from_raw(map.next_value()?)),
                        Field::Syscall if syscall.is_none() => syscall = Some(map.next_value()?),
                        Field::Args if args.is_none() => {
                            args = Some(map.next_value::<Native<SyscallArgs>>()?.0);
                        }
                        Field::Result if result.is_none() => {
                            result = Some(map.next_value::<Native<RetCode>>()?.0);
                        }
                        Field::Duration if duration.is_none() => duration = Some(map.next_value()?),
                        _ => return Err(A::Error::custom("duplicate native syscall field")),
                    }
                }
                Ok(SyscallInfo {
                    typ: typ.ok_or_else(|| A::Error::missing_field("typ"))?,
                    pid: pid.ok_or_else(|| A::Error::missing_field("pid"))?,
                    syscall: syscall.ok_or_else(|| A::Error::missing_field("syscall"))?,
                    args: args.ok_or_else(|| A::Error::missing_field("args"))?,
                    result: result.ok_or_else(|| A::Error::missing_field("result"))?,
                    duration: duration.ok_or_else(|| A::Error::missing_field("duration"))?,
                })
            }
        }
        deserializer.deserialize_struct(
            "SyscallInfo",
            &["typ", "pid", "syscall", "args", "result", "duration"],
            NativeInfo,
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SyscallArgs")]
struct Arguments(
    #[serde(
        serialize_with = "serialize_arguments",
        deserialize_with = "deserialize_arguments"
    )]
    Vec<SyscallArg>,
);

#[derive(Serialize, Deserialize)]
#[serde(remote = "SyscallArg")]
enum Argument {
    Int(i64),
    Str(String),
    StrVec(Vec<String>, Option<usize>),
    Addr(usize),
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "RetCode")]
enum ResultCode {
    Ok(i32),
    Err(i32),
    Address(usize),
}

/// An upstream value routed through its remote adapter where serde needs a local type.
struct Native<T>(T);
impl Serialize for Native<&SyscallArg> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Argument::serialize(self.0, serializer)
    }
}
impl<'de> Deserialize<'de> for Native<SyscallArg> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Argument::deserialize(deserializer).map(Self)
    }
}
impl<'de> Deserialize<'de> for Native<SyscallArgs> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Arguments::deserialize(deserializer).map(Self)
    }
}
impl<'de> Deserialize<'de> for Native<RetCode> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ResultCode::deserialize(deserializer).map(Self)
    }
}

/// Upstream's `SYSCALL` tag, validated without allocating.
struct Tag;
impl<'de> DeserializeSeed<'de> for Tag {
    type Value = &'static str;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}
impl Visitor<'_> for Tag {
    type Value = &'static str;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the upstream SYSCALL tag")
    }
    fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
        if value == "SYSCALL" {
            Ok("SYSCALL")
        } else {
            Err(E::custom("invalid syscall tag"))
        }
    }
}

/// Native syscall arguments, one remote-adapted element at a time.
struct ArgumentsVisitor;
impl<'de> Visitor<'de> for ArgumentsVisitor {
    type Value = Vec<SyscallArg>;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native syscall arguments")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut args = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(6));
        while let Some(Native(arg)) = sequence.next_element::<Native<SyscallArg>>()? {
            args.push(arg);
        }
        Ok(args)
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's serialize_with hands every field over by reference"
)]
fn serialize_pid<S: Serializer>(pid: &Pid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_i32(pid.as_raw())
}

fn serialize_arguments<S: Serializer>(
    args: &[SyscallArg],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(args.iter().map(Native))
}

fn deserialize_arguments<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SyscallArg>, D::Error> {
    deserializer.deserialize_seq(ArgumentsVisitor)
}
