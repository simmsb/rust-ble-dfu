//! Implements serialization of "init packets", which contain firmware metadata and precede the
//! actual firmware upload.
//!
//! The full init command format is defined [here][init].
//!
//! [init]: https://github.com/tmael/nRF5_SDK/blob/master/components/libraries/bootloader/dfu/dfu-cc.proto

use rohs::WireType;
use sha2::{Digest, Sha256};

/// Tiny protobuf writer shim, free of `Pb`.
mod rohs {
    use std::mem;

    pub enum WireType {
        Varint = 0,
        LengthDelimited = 2,
    }

    pub trait Value {
        const TYPE: WireType;
        fn write(&self, writer: &mut MessageWriter);
    }

    pub trait Message {
        fn write(&self, writer: &mut MessageWriter);
    }

    impl<M: Message> Value for M {
        const TYPE: WireType = WireType::LengthDelimited;

        fn write(&self, writer: &mut MessageWriter) {
            // Nested messages are prefixed with their encoded length.

            let prev_buf = mem::replace(&mut writer.buf, Vec::new());
            <M as Message>::write(self, writer);
            let message = mem::replace(&mut writer.buf, prev_buf);
            writer.write_varint(message.len() as u64);
            writer.buf.extend(message);
        }
    }

    impl Value for bool {
        const TYPE: WireType = WireType::Varint;

        fn write(&self, writer: &mut MessageWriter) {
            writer.write_varint(*self as _);
        }
    }

    impl Value for u32 {
        const TYPE: WireType = WireType::Varint;

        fn write(&self, writer: &mut MessageWriter) {
            writer.write_varint(*self as _);
        }
    }

    impl Value for [u8] {
        const TYPE: WireType = WireType::LengthDelimited;

        fn write(&self, writer: &mut MessageWriter) {
            writer.write_varint(self.len() as _);
            writer.buf.extend(self.iter().copied());
        }
    }

    pub struct MessageWriter {
        buf: Vec<u8>,
    }

    impl MessageWriter {
        pub fn new() -> Self {
            Self { buf: Vec::new() }
        }

        pub fn write_field<V: Value + ?Sized>(&mut self, name: &str, field_number: u32, value: &V) {
            // `name` is only for documentation purposes
            let _ = name;

            let wire_type = V::TYPE as u64;
            let key = (u64::from(field_number) << 3) | wire_type;
            self.write_varint(key);
            value.write(self);
        }

        pub fn write_opt_field<V: Value>(
            &mut self,
            name: &str,
            field_number: u32,
            value: &Option<V>,
        ) {
            if let Some(value) = value {
                self.write_field(name, field_number, value);
            }
        }

        pub fn write_varint(&mut self, varint: u64) {
            leb128::write::unsigned(&mut self.buf, varint).unwrap();
        }
    }

    pub fn encode_message<M: Message>(message: &M) -> Vec<u8> {
        let mut w = MessageWriter::new();
        message.write(&mut w);
        w.buf
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum FwType {
    Application = 0,
    Softdevice = 1,
    Bootloader = 2,
    SoftdeviceAndBootloader = 3,
}

impl rohs::Value for FwType {
    const TYPE: WireType = WireType::Varint;

    fn write(&self, writer: &mut rohs::MessageWriter) {
        writer.write_varint(*self as _);
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum HashType {
    NoHash = 0,
    Crc = 1,
    Sha128 = 2,
    /// This is the *only* hash type the stock bootloader accepts.
    Sha256 = 3,
    Sha512 = 4,
}

impl rohs::Value for HashType {
    const TYPE: WireType = WireType::Varint;

    fn write(&self, writer: &mut rohs::MessageWriter) {
        writer.write_varint(*self as _);
    }
}

struct Hash<'a> {
    hash_type: HashType,
    hash: &'a [u8],
}

impl rohs::Message for Hash<'_> {
    fn write(&self, writer: &mut rohs::MessageWriter) {
        writer.write_field("hash_type", 1, &self.hash_type);
        writer.write_field("hash", 2, self.hash);
    }
}

struct InitCommand<'a> {
    // FIXME: expected structure is unclear here, all fields are optional in the upstream spec.
    // We just support the bare minimum.
    fw_version: u32,
    /// Marked as optional, but omitting it results in `InitCommandInvalid`.
    hw_version: u32,
    fw_type: FwType,
    sd_size: u32,
    bl_size: u32,
    /// Size of the flashed app image (total size of all data objects that follow).
    app_size: u32,
    /// Marked as optional in the proto file, but seems to be required.
    hash: Hash<'a>,
    is_debug: Option<bool>,
}

impl rohs::Message for InitCommand<'_> {
    fn write(&self, writer: &mut rohs::MessageWriter) {
        writer.write_field("fw_version", 1, &self.fw_version);
        writer.write_field("hw_version", 2, &self.hw_version);
        writer.write_field("type", 4, &self.fw_type);
        writer.write_field("sd_size", 5, &self.sd_size);
        writer.write_field("bl_size", 6, &self.bl_size);
        writer.write_field("app_size", 7, &self.app_size);
        writer.write_field("hash", 8, &self.hash);
        writer.write_opt_field("is_debug", 9, &self.is_debug);
    }
}

enum Command<'a> {
    InitCommand(InitCommand<'a>),
}

impl rohs::Message for Command<'_> {
    fn write(&self, writer: &mut rohs::MessageWriter) {
        match self {
            Command::InitCommand(cmd) => {
                writer.write_field("op_code", 1, &1);
                writer.write_field("init", 2, cmd);
            }
        }
    }
}

/// This is the outermost message, which will actually be sent to the bootloader.
enum Packet<'a> {
    Command(Command<'a>),
    // Missing: SignedCommand
}

impl rohs::Message for Packet<'_> {
    fn write(&self, writer: &mut rohs::MessageWriter) {
        match self {
            Packet::Command(cmd) => {
                writer.write_field("command", 1, cmd);
            }
        }
    }
}

pub fn build_init_packet(image: &[u8]) -> Vec<u8> {
    let mut hash = {
        let mut hasher = Sha256::new();
        hasher.update(image);
        hasher.finalize()
    };
    let hash = &mut *hash;
    // For some reason, Nordic insists on transmitting (and displaying) the hash in little-endian
    // byte order, unlike the entire rest of the industry.
    hash.reverse();

    tracing::debug!(
        "image size: {} Bytes ({} KiB)",
        image.len(),
        image.len() / 1024,
    );
    tracing::debug!(
        "image hash: {}",
        hash.iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<Vec<_>>()
            .join("")
    );

    let packet = Packet::Command(Command::InitCommand(InitCommand {
        fw_version: 0,
        // 52 is the default, the docs do not recommend using it, but it's unclear how to
        // accomplish that.
        hw_version: 52,
        fw_type: FwType::Application,
        sd_size: 0,
        bl_size: 0,
        app_size: image.len() as _,
        hash: Hash {
            hash_type: HashType::Sha256,
            hash,
        },
        is_debug: Some(false),
    }));

    rohs::encode_message(&packet)
}
