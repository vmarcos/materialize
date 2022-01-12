// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types for the persist crate.

#![warn(missing_docs)]
#![warn(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::io;

mod codec_impls;

/// Encoding and decoding operations for a type usable as a persisted key or
/// value.
pub trait Codec: Sized + 'static {
    /// Name of the codec.
    ///
    /// This name is stored for the key and value when a stream is first created
    /// and the same key and value codec must be used for that stream afterward.
    fn codec_name() -> String;
    /// Encode a key or value for permanent storage.
    ///
    /// This must perfectly round-trip Self through [Codec::decode]. If the
    /// encode function for this codec ever changes, decode must be able to
    /// handle bytes output by all previous versions of encode.
    fn encode<E: for<'a> Extend<&'a u8>>(&self, buf: &mut E);
    /// Decode a key or value previous encoded with this codec's
    /// [Codec::encode].
    ///
    /// This must perfectly round-trip Self through [Codec::encode]. If the
    /// encode function for this codec ever changes, decode must be able to
    /// handle bytes output by all previous versions of encode.
    ///
    /// It should also gracefully handle data encoded by future versions of
    /// encode (likely with an error).
    //
    // TODO: Mechanically, this could return a ref to the original bytes
    // without any copies, see if we can make the types work out for that.
    fn decode<'a>(buf: &'a [u8]) -> Result<Self, String>;
}

/// An adaptor to implement [io::Write] for Extend<&u8>.
///
/// This is a helper for implementations of Codec that internally need a
/// [io::Write]. Writes and flushes are guaranteed to succeed.
pub struct ExtendWriteAdapter<'e, E>(pub &'e mut E);

impl<'e, E: for<'a> Extend<&'a u8>> io::Write for ExtendWriteAdapter<'e, E> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.0.extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}
