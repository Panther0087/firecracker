// Portions Copyright 2019 Red Hat, Inc.
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Define the ByteValued trait to mark that it is safe to instantiate the struct with random data.

/// Types for which it is safe to initialize from raw data.
///
/// A type `T` is `ByteValued` if and only if it can be initialized by reading its contents from a
/// byte array.  This is generally true for all plain-old-data structs.  It is notably not true for
/// any type that includes a reference.
///
/// Implementing this trait guarantees that it is safe to instantiate the struct with random data.
pub unsafe trait ByteValued: Copy + Default + Send + Sync {}

// All intrinsic types and arrays of intrinsic types are ByteValued. They are just numbers.
macro_rules! byte_valued_array {
    ($T:ty, $($N:expr)+) => {
        $(
            unsafe impl ByteValued for [$T; $N] {}
        )+
    }
}
macro_rules! byte_valued_type {
    ($T:ty) => {
        unsafe impl ByteValued for $T {}
        byte_valued_array! {
            $T,
            0  1  2  3  4  5  6  7  8  9
            10 11 12 13 14 15 16 17 18 19
            20 21 22 23 24 25 26 27 28 29
            30 31 32
        }
    };
}
byte_valued_type!(u8);
byte_valued_type!(u16);
byte_valued_type!(u32);
byte_valued_type!(u64);
byte_valued_type!(usize);
byte_valued_type!(i8);
byte_valued_type!(i16);
byte_valued_type!(i32);
byte_valued_type!(i64);
byte_valued_type!(isize);

/// A container to host a range of bytes and access its content.
///
/// Candidates which may implement this trait include:
/// - anonymous memory mappings
/// - memory mapped files
pub trait Bytes<A> {
    /// Associated error codes
    type E;

    /// Writes a slice into the container at the specified address.
    ///
    /// Returns the number of bytes written. The number of bytes written can
    /// be less than the length of the slice if there isn't enough room in the
    /// container.
    fn write_slice(&self, buf: &[u8], addr: A) -> Result<usize, Self::E>;

    /// Reads from the container at the specified address into a buffer.
    ///
    /// Returns the number of bytes read.  The number of bytes read can
    /// be less than the length of the slice if there isn't enough room in the
    /// container.
    fn read_slice(&self, buf: &mut [u8], addr: A) -> Result<usize, Self::E>;

    /// Reads an object from the container at the given address.
    ///
    /// Reading from a volatile area isn't strictly safe as it could change mid-read.
    /// However, as long as the type T is plain old data and can handle random initialization,
    /// everything will be OK.
    ///
    /// Caller needs to guarantee that the object does not cross the container
    /// boundary, otherwise it fails.
    fn read_obj<T: ByteValued>(&self, addr: A) -> Result<T, Self::E>;
}
