use loupe::MemoryUsage;
use rkyv::{
    archived_value,
    de::{adapters::SharedDeserializerAdapter, deserializers::AllocDeserializer},
    ser::adapters::SharedSerializerAdapter,
    ser::{serializers::WriteSerializer, Serializer as RkyvSerializer},
    Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize,
};
use serde::{Deserialize, Serialize};
use std::error::Error;
use wasmer_compiler::{
    CompileModuleInfo, CompiledFunctionFrameInfo, SectionIndex, Symbol, SymbolRegistry,
};
use wasmer_engine::{DeserializeError, SerializeError};
use wasmer_types::entity::{EntityRef, PrimaryMap};
use wasmer_types::{FunctionIndex, LocalFunctionIndex, OwnedDataInitializer, SignatureIndex};

fn to_serialize_error(err: impl Error) -> SerializeError {
    SerializeError::Generic(format!("{}", err))
}

/// Serializable struct that represents the compiled metadata.
#[derive(
    Clone,
    Serialize,
    Deserialize,
    Debug,
    MemoryUsage,
    RkyvSerialize,
    RkyvDeserialize,
    Archive,
    PartialEq,
    Eq,
)]
pub struct ModuleMetadata {
    pub compile_info: CompileModuleInfo,
    pub prefix: String,
    pub data_initializers: Box<[OwnedDataInitializer]>,
    pub frame_infos: PrimaryMap<LocalFunctionIndex, CompiledFunctionFrameInfo>,
}

#[derive(MemoryUsage)]
pub struct ModuleMetadataSymbolRegistry {
    pub prefix: String,
}

cfg_if::cfg_if! {
    if #[cfg(target_endian = "big")] {
        const HOST_ENDIAN: u8 = b'b';
    } else if #[cfg(target_endian = "little")] {
        const HOST_ENDIAN: u8 = b'l';
    }
    else {
        compile_error!("Endian isn't big or little?");
    }
}

impl ModuleMetadata {
    pub fn get_symbol_registry(&self) -> ModuleMetadataSymbolRegistry {
        ModuleMetadataSymbolRegistry {
            prefix: self.prefix.clone(),
        }
    }

    /// Serialize the Metadata into bytes
    /// The bytes will have the following format:
    /// RKYV serialization (any length) + POS (8 bytes) + Endian (1 byte)
    pub fn serialize(&self) -> Result<Vec<u8>, SerializeError> {
        let mut serializer = SharedSerializerAdapter::new(WriteSerializer::new(vec![]));
        let pos = serializer
            .serialize_value(self)
            .map_err(to_serialize_error)? as u64;
        let mut serialized_data = serializer.into_inner().into_inner();
        serialized_data.extend_from_slice(&pos.to_le_bytes());
        serialized_data.extend_from_slice(&[HOST_ENDIAN]);
        Ok(serialized_data)
    }

    /// Deserialize the Metadata from a slice.
    /// The slice must have the following format:
    /// RKYV serialization (any length) + POS (8 bytes) + Endian (1 byte)
    ///
    /// # Safety
    ///
    /// This method is unsafe since it deserializes data directly
    /// from memory.
    /// Right now we are not doing any extra work for validation, but
    /// `rkyv` has an option to do bytecheck on the serialized data before
    /// serializing (via `rkyv::check_archived_value`).
    pub unsafe fn deserialize(metadata_slice: &[u8]) -> Result<Self, DeserializeError> {
        let archived = Self::archive_from_slice(metadata_slice)?;
        Self::deserialize_from_archive(archived)
    }

    /// # Safety
    ///
    /// This method is unsafe.
    /// Please check `ModuleMetadata::deserialize` for more details.
    unsafe fn archive_from_slice<'a>(
        metadata_slice: &'a [u8],
    ) -> Result<&'a ArchivedModuleMetadata, DeserializeError> {
        if metadata_slice.len() < 9 {
            return Err(DeserializeError::Incompatible(
                "invalid serialized data".into(),
            ));
        }
        let mut pos: [u8; 8] = Default::default();
        let endian = metadata_slice[metadata_slice.len() - 1];
        if endian != HOST_ENDIAN {
            return Err(DeserializeError::Incompatible(
                format!(
                    "incompatible endian. Received {} but expected {}",
                    endian, HOST_ENDIAN
                )
                .into(),
            ));
        }
        pos.copy_from_slice(&metadata_slice[metadata_slice.len() - 9..metadata_slice.len() - 1]);
        let pos: u64 = u64::from_le_bytes(pos);
        Ok(archived_value::<ModuleMetadata>(
            &metadata_slice[..metadata_slice.len() - 9],
            pos as usize,
        ))
    }

    pub fn deserialize_from_archive(
        archived: &ArchivedModuleMetadata,
    ) -> Result<Self, DeserializeError> {
        let mut deserializer = SharedDeserializerAdapter::new(AllocDeserializer);
        RkyvDeserialize::deserialize(archived, &mut deserializer)
            .map_err(|e| DeserializeError::CorruptedBinary(format!("{:?}", e)))
    }
}

impl SymbolRegistry for ModuleMetadataSymbolRegistry {
    fn symbol_to_name(&self, symbol: Symbol) -> String {
        match symbol {
            Symbol::LocalFunction(index) => {
                format!("wasmer_function_{}_{}", self.prefix, index.index())
            }
            Symbol::Section(index) => format!("wasmer_section_{}_{}", self.prefix, index.index()),
            Symbol::FunctionCallTrampoline(index) => {
                format!(
                    "wasmer_trampoline_function_call_{}_{}",
                    self.prefix,
                    index.index()
                )
            }
            Symbol::DynamicFunctionTrampoline(index) => {
                format!(
                    "wasmer_trampoline_dynamic_function_{}_{}",
                    self.prefix,
                    index.index()
                )
            }
        }
    }

    fn name_to_symbol(&self, name: &str) -> Option<Symbol> {
        if let Some(index) = name.strip_prefix(&format!("wasmer_function_{}_", self.prefix)) {
            index
                .parse::<u32>()
                .ok()
                .map(|index| Symbol::LocalFunction(LocalFunctionIndex::from_u32(index)))
        } else if let Some(index) = name.strip_prefix(&format!("wasmer_section_{}_", self.prefix)) {
            index
                .parse::<u32>()
                .ok()
                .map(|index| Symbol::Section(SectionIndex::from_u32(index)))
        } else if let Some(index) =
            name.strip_prefix(&format!("wasmer_trampoline_function_call_{}_", self.prefix))
        {
            index
                .parse::<u32>()
                .ok()
                .map(|index| Symbol::FunctionCallTrampoline(SignatureIndex::from_u32(index)))
        } else if let Some(index) = name.strip_prefix(&format!(
            "wasmer_trampoline_dynamic_function_{}_",
            self.prefix
        )) {
            index
                .parse::<u32>()
                .ok()
                .map(|index| Symbol::DynamicFunctionTrampoline(FunctionIndex::from_u32(index)))
        } else {
            None
        }
    }
}
