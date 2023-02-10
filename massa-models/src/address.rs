// Copyright (c) 2022 MASSA LABS <info@massa.net>

use crate::error::ModelsError;
use crate::prehash::PreHashed;
use crate::slot::{Slot, SLOT_KEY_SIZE, SlotDeserializer};
use massa_hash::{Hash, HashDeserializer, HASH_SIZE_BYTES};
use massa_serialization::{
    DeserializeError, Deserializer, SerializeError, Serializer, U64VarIntDeserializer, U64VarIntSerializer,
};
use massa_signature::{PublicKey, PublicKeyV0, PublicKeyV1};
/*use nom::branch::alt;
use nom::combinator::verify;
use nom::sequence::preceded;*/
use nom::error::{context, ContextError, ErrorKind, ParseError};
use nom::{IResult, Parser};
use serde::{Deserialize, Serialize};
use std::ops::Bound::{Excluded,Included};
use std::str::FromStr;
use transition::Versioned;

/// Top level address representation that can differentiate between User and SC address
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Address {
    #[allow(missing_docs)]
    User(UserAddress),
    #[allow(missing_docs)]
    SC(SCAddress),
}

#[transition::versioned(versions("0", "1"))]
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SCAddress {
    /// The hash of the data
    #[transition::field(versions("0"))]
    pub hash: Hash,

    /// The data directly
    #[transition::field(versions("1"))]
    pub slot: Slot,
    
    /// The data directly
    #[transition::field(versions("1"))]
    pub created_addr_index: u64,
    
    /// The data directly
    #[transition::field(versions("1"))]
    pub read_only: bool
}

/// Derived from a public key.
#[transition::versioned(versions("0", "1"))]
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UserAddress(pub Hash);

const ADDRESS_PREFIX: char = 'A';
// serialized with varint
const USER_PREFIX: u64 = 0;
const SC_PREFIX: u64 = 1;

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Address::User(address) => address.fmt(f),
            Address::SC(address) => address.fmt(f),
        }
    }
}

impl std::fmt::Display for UserAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            UserAddress::UserAddressV0(address) => address.fmt(f),
            UserAddress::UserAddressV1(address) => address.fmt(f),
        }
    }
}

impl std::fmt::Display for SCAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SCAddress::SCAddressV0(address) => address.fmt(f),
            SCAddress::SCAddressV1(address) => address.fmt(f),
        }
    }
}

#[transition::impl_version(versions("0", "1"))]
impl std::fmt::Display for UserAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let u64_serializer = U64VarIntSerializer::new();
        let mut bytes: Vec<u8> = Vec::new();
        u64_serializer
            .serialize(&Self::VERSION, &mut bytes)
            .map_err(|_| std::fmt::Error)?;
        bytes.extend(self.0.to_bytes());
        write!(
            f,
            "{}{}{}",
            ADDRESS_PREFIX,
            'U',
            bs58::encode(bytes).with_check().into_string()
        )
    }
}

#[transition::impl_version(versions("0"))]
impl std::fmt::Display for SCAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let u64_serializer = U64VarIntSerializer::new();
        let mut bytes: Vec<u8> = Vec::new();
        u64_serializer
            .serialize(&Self::VERSION, &mut bytes)
            .map_err(|_| std::fmt::Error)?;
        bytes.extend(self.hash.to_bytes());
        write!(
            f,
            "{}{}{}",
            ADDRESS_PREFIX,
            'S',
            bs58::encode(bytes).with_check().into_string()
        )
    }
}

#[transition::impl_version(versions("1"))]
impl std::fmt::Display for SCAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let u64_serializer = U64VarIntSerializer::new();
        let mut bytes: Vec<u8> = Vec::new();
        u64_serializer
            .serialize(&Self::VERSION, &mut bytes)
            .map_err(|_| std::fmt::Error)?;
            bytes.extend(self.slot.to_bytes_key());
            bytes.extend(self.created_addr_index.to_be_bytes());
            if self.read_only {
                bytes.push(0u8);
            } else {
                bytes.push(1u8);
            }

        write!(
            f,
            "{}{}{}",
            ADDRESS_PREFIX,
            'S',
            bs58::encode(bytes).with_check().into_string()
        )
    }
}

// See https://github.com/massalabs/massa/pull/3479#issuecomment-1408694720
// as to why more information is not provided
impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl ::serde::Serialize for Address {
    fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Address::User(address) => address.serialize(s),
            Address::SC(address) => address.serialize(s),
        }
    }
}

impl ::serde::Serialize for UserAddress {
    fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            UserAddress::UserAddressV0(address) => address.serialize(s),
            UserAddress::UserAddressV1(address) => address.serialize(s),
        }
    }
}

impl ::serde::Serialize for SCAddress {
    fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SCAddress::SCAddressV0(address) => address.serialize(s),
            SCAddress::SCAddressV1(address) => address.serialize(s),
        }
    }
}

#[transition::impl_version(versions("0", "1"))]
impl ::serde::Serialize for UserAddress {
    fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(&self.to_string())
        } else {
            s.serialize_bytes(&self.to_prefixed_bytes())
        }
    }
}

#[transition::impl_version(versions("0", "1"))]
impl ::serde::Serialize for SCAddress {
    fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(&self.to_string())
        } else {
            s.serialize_bytes(&self.to_prefixed_bytes())
        }
    }
}

impl<'de> ::serde::Deserialize<'de> for Address {
    fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<Address, D::Error> {
        if d.is_human_readable() {
            struct AddressVisitor;

            impl<'de> ::serde::de::Visitor<'de> for AddressVisitor {
                type Value = Address;

                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    formatter.write_str("A + {U | S} + base58::encode(version + hash)")
                }

                fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
                where
                    E: ::serde::de::Error,
                {
                    if let Ok(v_str) = std::str::from_utf8(v) {
                        Address::from_str(v_str).map_err(E::custom)
                    } else {
                        Err(E::invalid_value(::serde::de::Unexpected::Bytes(v), &self))
                    }
                }

                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
                where
                    E: ::serde::de::Error,
                {
                    Address::from_str(v).map_err(E::custom)
                }
            }
            d.deserialize_str(AddressVisitor)
        } else {
            struct BytesVisitor;

            impl<'de> ::serde::de::Visitor<'de> for BytesVisitor {
                type Value = Address;

                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    formatter.write_str("[u64varint-of-addr-variant][u64varint-of-version][bytes]")
                }

                fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
                where
                    E: ::serde::de::Error,
                {
                    AddressDeserializer::new()
                        .deserialize::<DeserializeError>(v)
                        .map_err(E::custom)
                        .map(|r| r.1)
                }
            }

            d.deserialize_bytes(BytesVisitor)
        }
    }
}

impl FromStr for Address {
    type Err = ModelsError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = Err(ModelsError::AddressParseError);

        // Handle the prefix ("A{U|S}")
        let mut chars = s.chars();
        let Some(ADDRESS_PREFIX) = chars.next() else {
            return err;
        };
        let Some(pref) = chars.next() else {
            return err;
        };

        let res = match pref {
            'U' => Address::User(UserAddress::from_str_without_prefixed_type(&s[2..])?),
            'S' => Address::SC(SCAddress::from_str_without_prefixed_type(&s[2..])?),
            _ => return err,
        };
        Ok(res)
    }
}

impl Address {
    pub fn get_thread(&self, thread_count: u8) -> Option<u8> {
        match self {
            Address::User(addr) => Some(addr.get_thread(thread_count)),
            Address::SC(addr) => None,
        }
    }

    /// Computes the address associated with the given public key.
    /// Depends on the Public Key version
    pub fn from_public_key(public_key: &PublicKey) -> Self {
        Address::User(UserAddress::from_public_key(public_key))
    }

    pub fn to_prefixed_bytes(&self) -> Vec<u8> {
        match self {
            Address::User(addr) => addr.to_prefixed_bytes(),
            Address::SC(addr) => addr.to_prefixed_bytes(),
        }
    }
}

impl UserAddress {
    
    /// Gets the associated thread. Depends on the `thread_count`
    fn get_thread(&self, thread_count: u8) -> u8 {
        match self {
            UserAddress::UserAddressV0(addr) => addr.get_thread(thread_count),
            UserAddress::UserAddressV1(addr) => addr.get_thread(thread_count),
        }
    }

    /// Computes the address associated with the given public key.
    /// Depends on the Public Key version
    fn from_public_key(public_key: &PublicKey) -> Self {
        //BuildVersions!(version, from_public_key(public_key), ModelsError::InvalidVersionError(format!("Address version {} doesn't exist.", version)));

        match public_key {
            PublicKey::PublicKeyV0(pk) => {
                UserAddressVariant!["0"](<UserAddress!["0"]>::from_public_key(pk))
            }
            PublicKey::PublicKeyV1(pk) => {
                UserAddressVariant!["1"](<UserAddress!["1"]>::from_public_key(pk))
            }
        }
    }

    fn from_str_without_prefixed_type(s: &str) -> Result<Self, ModelsError> {
        let decoded_bs58_check = bs58::decode(s)
            .with_check(None)
            .into_vec()
            .map_err(|_| ModelsError::AddressParseError)?;
        let u64_deserializer = U64VarIntDeserializer::new(Included(0), Included(u64::MAX));
        let (rest, version) = u64_deserializer
            .deserialize::<DeserializeError>(&decoded_bs58_check[..])
            .map_err(|_| ModelsError::AddressParseError)?;

        match version {
            <UserAddress!["0"]>::VERSION => Ok(UserAddressVariant!["0"](
                <UserAddress!["0"]>::from_bytes_without_version(rest)?,
            )),
            <UserAddress!["1"]>::VERSION => Ok(UserAddressVariant!["1"](
                <UserAddress!["1"]>::from_bytes_without_version(rest)?,
            )),
            _ => Err(ModelsError::AddressParseError),
        }
    }

    pub fn to_prefixed_bytes(&self) -> Vec<u8> {
        match self {
            UserAddress::UserAddressV0(addr) => addr.to_prefixed_bytes(),
            UserAddress::UserAddressV1(addr) => addr.to_prefixed_bytes(),
        }
    }
}

#[transition::impl_version(versions("0", "1"))]
impl UserAddress {
    pub fn get_version(&self) -> u64 {
        Self::VERSION
    }

    fn to_prefixed_bytes(&self) -> Vec<u8> {
        let mut buff = vec![];
        let addr_type_ser = U64VarIntSerializer::new();
        let addr_vers_ser = U64VarIntSerializer::new();
        addr_type_ser
            .serialize(&USER_PREFIX, &mut buff)
            .expect("impl always returns Ok(())");
        addr_vers_ser
            .serialize(&Self::VERSION, &mut buff)
            .expect("impl always returns Ok(())");
        buff.extend_from_slice(&self.0.to_bytes()[..]);
        buff
    }

    /// Gets the associated thread. Depends on the `thread_count`
    fn get_thread(&self, thread_count: u8) -> u8 {
        (self.0.to_bytes()[0])
            .checked_shr(8 - thread_count.trailing_zeros())
            .unwrap_or(0)
    }

    fn from_bytes_without_version(data: &[u8]) -> Result<UserAddress, ModelsError> {
        Ok(UserAddress(Hash::from_bytes(&data.try_into().map_err(
            |_| {
                ModelsError::BufferError(format!(
                    "expected a buffer of size {}, but found a size of {}",
                    HASH_SIZE_BYTES,
                    &data.len()
                ))
            },
        )?)))
    }
}


#[transition::impl_version(versions("0", "1"), structures("UserAddress", "PublicKey"))]
impl UserAddress {
    /// Computes address associated with given public key
    pub fn from_public_key(public_key: &PublicKey) -> Self {
        UserAddress(Hash::compute_from(&public_key.to_bytes()))
    }
}

#[transition::impl_version(versions("0"))]
impl UserAddress {

}

#[transition::impl_version(versions("1"))]
impl UserAddress {

}


impl SCAddress {
    fn from_str_without_prefixed_type(s: &str) -> Result<Self, ModelsError> {
        let decoded_bs58_check = bs58::decode(s)
            .with_check(None)
            .into_vec()
            .map_err(|_| ModelsError::AddressParseError)?;
        let u64_deserializer = U64VarIntDeserializer::new(Included(0), Included(u64::MAX));
        let (rest, version) = u64_deserializer
            .deserialize::<DeserializeError>(&decoded_bs58_check[..])
            .map_err(|_| ModelsError::AddressParseError)?;

        match version {
            <SCAddress!["0"]>::VERSION => Ok(SCAddressVariant!["0"](
                <SCAddress!["0"]>::from_bytes_without_version(rest)?,
            )),
            <SCAddress!["1"]>::VERSION => Ok(SCAddressVariant!["1"](
                <SCAddress!["1"]>::from_bytes_without_version(rest)?,
            )),
            _ => Err(ModelsError::AddressParseError),
        }
    }

    pub fn to_prefixed_bytes(&self) -> Vec<u8> {
        match self {
            SCAddress::SCAddressV0(addr) => addr.to_prefixed_bytes(),
            SCAddress::SCAddressV1(addr) => addr.to_prefixed_bytes(),
        }
    }

    pub fn from_bytes_without_version(version: u64, data: &[u8]) -> Result<SCAddress, ModelsError> {
        match version {
            <SCAddress!["0"]>::VERSION => Ok(SCAddressVariant!["0"](<SCAddress!["0"]>::from_bytes_without_version(data)?,)),
            <SCAddress!["1"]>::VERSION => Ok(SCAddressVariant!["1"](<SCAddress!["1"]>::from_bytes_without_version(data)?,)),
            _ => Err(ModelsError::AddressParseError),
        }
    }
}

#[transition::impl_version(versions("0", "1"))]
impl SCAddress {
    pub fn get_version(&self) -> u64 {
        Self::VERSION
    }
}

#[transition::impl_version(versions("0"))]
impl SCAddress {
    pub fn to_prefixed_bytes(&self) -> Vec<u8> {
        let mut buff = vec![];
        let addr_type_ser = U64VarIntSerializer::new();
        let addr_vers_ser = U64VarIntSerializer::new();
        addr_type_ser
            .serialize(&SC_PREFIX, &mut buff)
            .expect("impl always returns Ok(())");
        addr_vers_ser
            .serialize(&Self::VERSION, &mut buff)
            .expect("impl always returns Ok(())");
        buff.extend_from_slice(&self.hash.to_bytes()[..]);
        buff
    }

    fn from_bytes_without_version(data: &[u8]) -> Result<SCAddress, ModelsError> {
        Ok(SCAddress{hash: Hash::from_bytes(&data.try_into().map_err(
            |_| {
                ModelsError::BufferError(format!(
                    "expected a buffer of size {}, but found a size of {}",
                    HASH_SIZE_BYTES,
                    &data.len()
                ))
            },
        )?)})
    }
    
}

#[transition::impl_version(versions("1"))]
impl SCAddress {
    pub fn to_prefixed_bytes(&self) -> Vec<u8> {
        let mut buff = vec![];
        let addr_type_ser = U64VarIntSerializer::new();
        let addr_vers_ser = U64VarIntSerializer::new();
        addr_type_ser
            .serialize(&SC_PREFIX, &mut buff)
            .expect("impl always returns Ok(())");
        addr_vers_ser
            .serialize(&Self::VERSION, &mut buff)
            .expect("impl always returns Ok(())");
            buff.extend_from_slice(&self.slot.to_bytes_key()[..]);
            buff.extend_from_slice(&self.created_addr_index.to_be_bytes()[..]);
            if self.read_only {
                buff.push(0u8);
            } else {
                buff.push(1u8);
            }
        buff
    }

    fn from_bytes_without_version(data: &[u8]) -> Result<SCAddress, ModelsError> {

        if data.len() != SLOT_KEY_SIZE + 9
        {
            return Err(ModelsError::BufferError(format!(
                "expected a buffer of size  than {}, but found a size of {}",
                SLOT_KEY_SIZE + 9,
                &data.len()
            )));
        }

        let err = Err(ModelsError::BufferError(String::from("could not deserialize buffer from_bytes_without_version")));

        let (data_slot, rest) = data.split_at(SLOT_KEY_SIZE);
        let (data_created_addr_index, data_read_only) = rest.split_at(rest.len()-1);
        
        let slot = Slot::from_bytes_key(&data_slot.try_into().expect("Unexpected error"));
        let created_addr_index = u64::from_be_bytes(data_created_addr_index.try_into().expect("Unexpected error"));
        let read_only = match data_read_only {
            [0_u8] => { false },
            [1_u8] => { true },
            _ => { return err; },
        };

        Ok(SCAddress { slot, created_addr_index, read_only })
    }

}

/* /!\ SCAddressV1 not prehashed! */
impl PreHashed for Address {}


/// Serializer for `Address`
#[derive(Default, Clone)]
pub struct AddressSerializer {
    type_serializer: U64VarIntSerializer,
    version_serializer: U64VarIntSerializer,
}

impl AddressSerializer {
    /// Serializes an `Address` into a `Vec<u8>`
    pub fn new() -> Self {
        Self {
            type_serializer: U64VarIntSerializer::new(),
            version_serializer: U64VarIntSerializer::new(),
        }
    }
}

impl Serializer<Address> for AddressSerializer {
    fn serialize(&self, value: &Address, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        match value {
            Address::User(addr) => self.serialize(addr, buffer),
            Address::SC(addr) => self.serialize(addr, buffer),
        }
    }
}

impl Serializer<UserAddress> for AddressSerializer {
    fn serialize(&self, value: &UserAddress, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.type_serializer.serialize(&USER_PREFIX, buffer)?;
        match value {
            UserAddress::UserAddressV0(addr) => self.serialize(addr, buffer),
            UserAddress::UserAddressV1(addr) => self.serialize(addr, buffer),
        }
    }
}

#[transition::impl_version(versions("0", "1"), structures("UserAddress"))]
impl Serializer<UserAddress> for AddressSerializer {
    fn serialize(&self, value: &UserAddress, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.version_serializer
            .serialize(&value.get_version(), buffer)?;
        buffer.extend_from_slice(&value.0.into_bytes());
        Ok(())
    }
}

impl Serializer<SCAddress> for AddressSerializer {
    fn serialize(&self, value: &SCAddress, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.type_serializer.serialize(&SC_PREFIX, buffer)?;
        match value {
            SCAddress::SCAddressV0(addr) => self.serialize(addr, buffer),
            SCAddress::SCAddressV1(addr) => self.serialize(addr, buffer),
        }
    }
}

#[transition::impl_version(versions("0"), structures("SCAddress"))]
impl Serializer<SCAddress> for AddressSerializer {
    fn serialize(&self, value: &SCAddress, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.version_serializer
            .serialize(&value.get_version(), buffer)?;
        buffer.extend_from_slice(&value.hash.into_bytes());
        Ok(())
    }
}

#[transition::impl_version(versions("1"), structures("SCAddress"))]
impl Serializer<SCAddress> for AddressSerializer {
    fn serialize(&self, value: &SCAddress, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.version_serializer
            .serialize(&value.get_version(), buffer)?;
        buffer.extend_from_slice(&value.slot.to_bytes_key()[..]);
        buffer.extend_from_slice(&value.created_addr_index.to_be_bytes()[..]);
        if value.read_only {
            buffer.push(0u8);
        } else {
            buffer.push(1u8);
        }
        Ok(())
    }
}


/// Deserializer for `Address`
#[derive(Clone)]
pub struct AddressDeserializer {
    type_deserializer: U64VarIntDeserializer,
    version_deserializer: U64VarIntDeserializer,
    hash_deserializer: HashDeserializer,
    slot_deserializer: SlotDeserializer,
    u64_deserializer: U64Deserializer,
    u8_deserializer: U8Deserializer,
}

impl Default for AddressDeserializer {
    fn default() -> Self {
        AddressDeserializer::new()
    }
}

impl AddressDeserializer {
    /// Creates a new deserializer for `Address`
    pub const fn new() -> Self {
        Self {   
            type_deserializer: U64VarIntDeserializer::new(Included(0), Excluded(1)),
            version_deserializer: U64VarIntDeserializer::new(Included(0), Excluded(u64::MAX)),
            hash_deserializer: HashDeserializer::new(),
            slot_deserializer: SlotDeserializer::new((Included(0), Excluded(u64::MAX)), (Included(0), Excluded(u8::MAX))),
            u64_deserializer: U64Deserializer::new(),
            u8_deserializer: U8Deserializer::new(),
        }
    }
}

impl Deserializer<Address> for AddressDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], Address, E> {
        if buffer.len() < 2 {
            return Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof)));
        }
        let (rest, addr_type) = self.type_deserializer.deserialize(buffer)
        .map_err(|_: nom::Err<E>| {nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof))})?;
        match addr_type {
            USER_PREFIX => {
                let (rest, addr) = self.deserialize(&rest)?;
                Ok((rest, Address::User(addr)))
            }
            SC_PREFIX => {
                let (rest, addr) = self.deserialize(&buffer[1..])?;
                Ok((rest, Address::SC(addr)))
            }
            _ => Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof))),
        }
    }
}

impl Deserializer<UserAddress> for AddressDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], UserAddress, E> {
        if buffer.len() < 2 {
            return Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof)));
        }
        let (rest, addr_vers) = self.version_deserializer.deserialize(buffer)
        .map_err(|_: nom::Err<E>| {nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof))})?;
        match addr_vers {
            <UserAddress!["0"]>::VERSION => {
                let (rest, addr) = self.deserialize(&rest)?;
                Ok((rest, UserAddressVariant!["0"](addr)))
            }
            <UserAddress!["1"]>::VERSION => {
                let (rest, addr) = self.deserialize(&buffer[1..])?;
                Ok((rest, UserAddressVariant!["1"](addr)))
            }
            _ => Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof))),
        }
    }
}

#[transition::impl_version(versions("0", "1"), structures("UserAddress"))]
impl Deserializer<UserAddress> for AddressDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], UserAddress, E> {
        context("Failed UserAddress deserialization", |input| {
            self.hash_deserializer.deserialize(input)
        })
        .map(UserAddress)
        .parse(buffer)
    }
}

impl Deserializer<SCAddress> for AddressDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], SCAddress, E> {
        if buffer.len() < 2 {
            return Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof)));
        }
        let (rest, addr_vers) = self.version_deserializer.deserialize(buffer)
        .map_err(|_: nom::Err<E>| {nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof))})?;
        match addr_vers {
            <SCAddress!["0"]>::VERSION => {
                let (rest, addr) = self.deserialize(&rest)?;
                Ok((rest, SCAddressVariant!["0"](addr)))
            }
            <SCAddress!["1"]>::VERSION => {
                let (rest, addr) = self.deserialize(&buffer[1..])?;
                Ok((rest, SCAddressVariant!["1"](addr)))
            }
            _ => Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof))),
        }
    }
}

#[transition::impl_version(versions("0"), structures("SCAddress"))]
impl Deserializer<SCAddress> for AddressDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], SCAddress, E> {
        context("Failed SCAddress deserialization", |input| {
            self.hash_deserializer.deserialize(input)
        })
        .map(|h| SCAddress{hash: h})
        .parse(buffer)
    }
}

#[transition::impl_version(versions("1"), structures("SCAddress"))]
impl Deserializer<SCAddress> for AddressDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], SCAddress, E> {
        context("Failed SCAddress deserialization", |input| {
            let (rest, slot) = self.slot_deserializer.deserialize(input)?;
            let (rest, created_addr_index) = self.u64_deserializer.deserialize(rest)?;
            let (rest, read_only_byte) = self.u8_deserializer.deserialize(rest)?;
            let read_only = match read_only_byte {
                0u8 => false,
                1u8 => true,
                _ => { return Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof)))}
            };
            
            Ok((rest, (slot, created_addr_index, read_only)))
        })
        .map(|(slot, created_addr_index, read_only)| SCAddress{slot, created_addr_index, read_only})
        .parse(buffer)
    }
}

/// Deserializer for `Address`
#[derive(Default, Clone)]
pub struct U64Deserializer {}

impl U64Deserializer {
    /// Creates a new deserializer for `Address`
    pub const fn new() -> Self {
        Self {}
    }
}
/// Deserializer for `Address`
#[derive(Default, Clone)]
pub struct U8Deserializer {}

impl U8Deserializer {
    /// Creates a new deserializer for `Address`
    pub const fn new() -> Self {
        Self {}
    }
}

impl Deserializer<u64> for U64Deserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], u64, E> {
        if buffer.len() < 8 {
            return Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof)));
        }
        let (u64_bytes, rest) = buffer.split_at(8);

        let u64 = u64::from_be_bytes(u64_bytes.try_into()
        .map_err(|_| nom::Err::Error(
            E::from_error_kind(buffer, ErrorKind::Eof)))?);
        Ok((rest, u64))
    }
}

impl Deserializer<u8> for U8Deserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], u8, E> {
        if buffer.len() < 2 {
            return Err(nom::Err::Error(E::from_error_kind(buffer, ErrorKind::Eof)));
        }
        let u8 = buffer[0];
        let rest = &buffer[1..];
        Ok((rest, u8))
    }
}

/// Info for a given address on a given cycle
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionAddressCycleInfo {
    /// cycle number
    pub cycle: u64,
    /// true if that cycle is final
    pub is_final: bool,
    /// `ok_count` blocks were created by this address during that cycle
    pub ok_count: u64,
    /// `ok_count` blocks were missed by this address during that cycle
    pub nok_count: u64,
    /// number of active rolls the address had at that cycle (if still available)
    pub active_rolls: Option<u64>,
}


#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_address() {
        
        let hash = massa_hash::Hash::compute_from(&"ADDR".as_bytes());

        let slot = Slot::new(10, 2);
        let created_addr_index = 13;
        let read_only = false;

        let user_addr_0 = Address::User(UserAddress::UserAddressV0(UserAddressV0(hash)));
        let user_addr_1 = Address::User(UserAddress::UserAddressV1(UserAddressV1(hash)));
        let sc_addr_0 = Address::SC(SCAddress::SCAddressV0(SCAddressV0 { hash }));
        let sc_addr_1 = Address::SC(SCAddress::SCAddressV1(SCAddressV1 { slot, created_addr_index, read_only }));

        println!("user_addr_0: {}", user_addr_0);
        println!("user_addr_1: {}", user_addr_1);
        println!("sc_addr_0: {}", sc_addr_0);
        println!("sc_addr_1: {}", sc_addr_1);

    }
}
