//! HAL blocking interface to the AES CCM mode encryption.
//!
//! Counter with CBC-MAC (CCM) mode is an authenticated encryption
//! algorithm designed to provide both authentication and confidentiality during data transfer.
//!
//! # Packet Format
//!
//! The packets, required by the methods in this module, need to be in a specific format, displayed
//! below:
//!
//! Cleartext packet:
//!
//! ```notrust
//! +----------+---------------+----------+------------------+
//! | S0       | Packet length | S1       | Payload          |
//! | (1 byte) | (1 byte)      | (1 byte) | (0 - 251* bytes) |
//! +----------+---------------+----------+------------------+
//! ```
//!
//! The contents of `S0` and `S1` are not relevant, but the fields must be present in the slice.
//! The encryption operation will append a four-byte MIC after the payload field and add four to the
//! `Payload length` field. Because of that, this module can only encrypt packets with payloads
//! lengths up to 251 bytes. The `cipher packet` slice passed to the encryption method must have
//! enough space for the `clear packet` plus MIC.
//!
//! Ciphertext packet:
//!
//! ```notrust
//! +----------+---------------+----------+-----------------+-------------+
//! | S0       | Packet length | S1       | Payload          | MIC        |
//! | (1 byte) | (1 byte)      | (1 byte) | (0 - 251* bytes) | (4 bytes)  |
//! +----------+---------------+----------+-----------------+-------------+
//! ```
//! The contents of `S0` and `S1` are not relevant, but the fields must be present in the slice. The
//! `Packet length` is the sum of the lengths of the `Payload` and `MIC`.
//! The decryption operation will also check the MIC field and return an error when it is invalid
//! and it will decrement the `Length` field by four. During decryption, the `clear text` slice does
//! not need to have space for the MIC field.
//!
//! * nRF51 devices only support payloads of up to 27 bytes.
//!
//! # Scratch Area
//!
//! The peripheral also needs an area in RAM to store temporary values used during
//! encryption/decryption. The scratch slice must have a minimum length of 43 bytes, or
//! (16 + `Packet Length`) bytes, whatever is largest.

use crate::{
    slice_in_ram,
    target::{AAR, CCM},
};
use core::sync::atomic::{compiler_fence, Ordering};

#[cfg(not(feature = "51"))]
use crate::target::ccm::mode::{DATARATE_A, LENGTH_A};

const MINIMUM_SCRATCH_AREA_SIZE: usize = 43;
const HEADER_SIZE: usize = 3;
const LENGTH_HEADER_INDEX: usize = 1;
const MIC_SIZE: usize = 4;
const MAXIMUM_LENGTH_5BITS: usize = 31;

// 39-bits counter
const MAXIMUM_COUNTER: u64 = 0x7F_FFFF_FFFF;

/// Data rate that CCM peripheral shall run in sync with.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum DataRate {
    _1Mbit,
    #[cfg(not(feature = "51"))]
    _2Mbit,
}

#[cfg(not(feature = "51"))]
impl From<DataRate> for DATARATE_A {
    fn from(data_rate: DataRate) -> Self {
        if data_rate == DataRate::_1Mbit {
            DATARATE_A::_1MBIT
        } else {
            DATARATE_A::_2MBIT
        }
    }
}

/// CCM error.
#[derive(Debug, PartialEq)]
pub enum CcmError {
    /// One or more buffers passed to CCM are not in RAM.
    BufferNotInRAM,
    /// Some bus conflict caused an error during encryption/decryption.
    EasyDMAError,
    /// The packet header contains an invalid length field.
    WrongPacketLength,
    /// The slice passed in for the scratch area is not big enough.
    InsufficientScratchArea,
    /// The MIC validation failed during decryption, this will always be true for cipher packets
    /// that have payload lengths of one to four (inclusive).
    InvalidMIC,
}

/// Data used for encryption/decryption.
///
/// It consists of a 128-bits key, a 39-bits counter, a direction bit and a 8-bytes initialization
/// vector. There are some reserved bits in this structure, the total size is 33 bytes.
///
/// The NONCE vector (as specified by the Bluetooth Core Specification) will be generated by
/// hardware based on this information.
#[derive(Debug, PartialEq)]
#[repr(C)]
pub struct CcmData {
    key: [u8; 16],
    packet_counter: [u8; 8],
    direction: u8,
    initialization_vector: [u8; 8],
}

impl CcmData {
    /// Creates a new `CcmData` instance.
    ///
    /// The direction bit and the counter value will be initialized to zero. Care must be taken when
    /// choosing an initialization vector, it must be sufficiently random.
    pub fn new(key: [u8; 16], initialization_vector: [u8; 8]) -> Self {
        Self {
            key,
            packet_counter: [0; 8],
            direction: 0,
            initialization_vector,
        }
    }

    /// Updates the key.
    #[inline(always)]
    pub fn set_key(&mut self, key: [u8; 16]) {
        self.key = key;
    }

    /// Updates the initialization vector.
    #[inline(always)]
    pub fn set_iv(&mut self, initialization_vector: [u8; 8]) {
        self.initialization_vector = initialization_vector;
    }

    /// Updates the direction bit.
    #[inline(always)]
    pub fn set_direction(&mut self, direction: bool) {
        self.direction = if direction { 1 } else { 0 };
    }

    /// Increments the counter. It will wrap around to zero at its maximum value.
    pub fn increment_counter(&mut self) {
        let mut counter = u64::from_le_bytes(self.packet_counter);
        if counter < MAXIMUM_COUNTER {
            counter += 1;
        } else {
            counter = 0;
        }
        self.packet_counter = counter.to_le_bytes();
    }

    /// Decrements the counter if the current value is bigger than zero.
    pub fn decrement_counter(&mut self) {
        let mut counter = u64::from_le_bytes(self.packet_counter);
        if counter > 0 {
            counter -= 1;
            self.packet_counter = counter.to_le_bytes();
        }
    }
}

/// A safe, blocking wrapper around the AES-CCM peripheral.
pub struct Ccm {
    regs: CCM,
    _aar: AAR,
}

impl Ccm {
    /// Inits the CCM peripheral. This method also demands ownership of the AAR peripheral, because
    /// it shares registers with the CCM.
    pub fn init(regs: CCM, arr: AAR, data_rate: DataRate) -> Self {
        arr.enable.write(|w| w.enable().disabled());

        // Disable all interrupts
        regs.intenclr
            .write(|w| w.endcrypt().clear().endksgen().clear().error().clear());

        // NOTE(unsafe) 1 is a valid pattern to write to this register
        regs.tasks_stop.write(|w| unsafe { w.bits(1) });

        // This register is shared with AAR, reset it and write the chosen data rate
        #[cfg(not(feature = "51"))]
        regs.mode.write(|w| w.datarate().variant(data_rate.into()));

        #[cfg(feature = "51")]
        let _ = data_rate;

        regs.enable.write(|w| w.enable().enabled());

        Self { regs, _aar: arr }
    }

    /// Encrypts a packet and generates a MIC.
    ///
    /// The generated MIC will be placed after the payload in the `cipher_packet`. The slices
    /// passed to this method must have the correct size, for more information refer to the module
    /// level documentation. The counter in `ccm_data` will be incremented if the operation
    /// succeeds. All parameters passed to this method must reside in RAM.
    pub fn encrypt_packet(
        &mut self,
        ccm_data: &mut CcmData,
        clear_packet: &[u8],
        cipher_packet: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<(), CcmError> {
        if !(slice_in_ram(clear_packet) && slice_in_ram(cipher_packet) && slice_in_ram(scratch)) {
            return Err(CcmError::BufferNotInRAM);
        }

        if clear_packet.len() < HEADER_SIZE || cipher_packet.len() < HEADER_SIZE {
            return Err(CcmError::WrongPacketLength);
        }

        let payload_len = clear_packet[LENGTH_HEADER_INDEX] as usize;

        // Shortcut, CCM won't encrypt packet with empty payloads, it will just copy the header
        if payload_len == 0 {
            (&mut cipher_packet[..HEADER_SIZE]).copy_from_slice(&clear_packet[..HEADER_SIZE]);
            return Ok(());
        }

        if clear_packet.len() < payload_len + HEADER_SIZE
            || cipher_packet.len() < payload_len + HEADER_SIZE + MIC_SIZE
            || payload_len + MIC_SIZE > u8::MAX as usize
        {
            return Err(CcmError::WrongPacketLength);
        }

        if scratch.len() < (payload_len + 16).max(MINIMUM_SCRATCH_AREA_SIZE) {
            return Err(CcmError::InsufficientScratchArea);
        }

        #[cfg(feature = "51")]
        {
            if payload_len > MAXIMUM_LENGTH_5BITS - MIC_SIZE {
                return Err(CcmError::WrongPacketLength);
            }
        }

        #[cfg(not(feature = "51"))]
        let length_variant = if payload_len <= MAXIMUM_LENGTH_5BITS - MIC_SIZE {
            LENGTH_A::DEFAULT
        } else {
            #[cfg(any(feature = "52840", feature = "52833", feature = "52810"))]
            // NOTE(unsafe) Any 8bits pattern is safe to write to this register
            self.regs
                .maxpacketsize
                .write(|w| unsafe { w.maxpacketsize().bits(payload_len as u8) });

            LENGTH_A::EXTENDED
        };

        #[cfg(feature = "51")]
        self.regs.mode.write(|w| w.mode().encryption());

        #[cfg(not(feature = "51"))]
        self.regs
            .mode
            .modify(|_, w| w.mode().encryption().length().variant(length_variant));

        // Setup the pointers
        // NOTE(unsafe) These addreses are in RAM, checked above
        unsafe {
            self.regs
                .cnfptr
                .write(|w| w.bits(ccm_data as *mut _ as u32));

            self.regs
                .inptr
                .write(|w| w.bits(clear_packet.as_ptr() as u32));
            self.regs
                .outptr
                .write(|w| w.bits(cipher_packet.as_mut_ptr() as u32));
            self.regs
                .scratchptr
                .write(|w| w.bits(scratch.as_mut_ptr() as u32));
        }

        // Clear events
        self.regs.events_endcrypt.reset();
        self.regs.events_error.reset();
        self.regs.events_endksgen.reset();

        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);

        // Start key generation
        // NOTE(unsafe) 1 is a valid pattern to write to this register
        self.regs.tasks_ksgen.write(|w| unsafe { w.bits(1) });

        while self.regs.events_endksgen.read().bits() == 0 {}

        // NOTE(unsafe) 1 is a valid pattern to write to this register
        self.regs.tasks_crypt.write(|w| unsafe { w.bits(1) });

        while self.regs.events_endcrypt.read().bits() == 0
            && self.regs.events_error.read().bits() == 0
        {}

        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        if self.regs.events_error.read().bits() == 1 {
            // It's ok to return here, the events will be cleared before the next
            // encryption/decryption
            return Err(CcmError::EasyDMAError);
        }
        ccm_data.increment_counter();
        Ok(())
    }

    /// Decrypts a packet and checks its MIC.
    ///
    /// This method will return an error if the MIC verification fails. The slices passed to this
    /// method must have the correct size, for more information refer to the module level
    /// documentation. The counter in `ccm_data` will be incremented if the operation succeeds. All
    /// parameters passed to this method must reside in RAM.
    pub fn decrypt_packet(
        &mut self,
        ccm_data: &mut CcmData,
        clear_packet: &mut [u8],
        cipher_packet: &[u8],
        scratch: &mut [u8],
    ) -> Result<(), CcmError> {
        if !(slice_in_ram(clear_packet) && slice_in_ram(cipher_packet) && slice_in_ram(scratch)) {
            return Err(CcmError::BufferNotInRAM);
        }

        if clear_packet.len() < HEADER_SIZE || cipher_packet.len() < HEADER_SIZE {
            return Err(CcmError::WrongPacketLength);
        }

        let payload_len = cipher_packet[LENGTH_HEADER_INDEX] as usize;

        // Shortcut, CCM won't decrypt packet with empty payloads, it will just copy the header
        if payload_len == 0 {
            (&mut clear_packet[..HEADER_SIZE]).copy_from_slice(&cipher_packet[..HEADER_SIZE]);
            return Ok(());
        }

        // Shorcut, CCM needs at least 5 bytes (1 byte payload + 4 bytes MIC), it will return a MIC
        // Error in that case, payload_len = 0 is an exception
        if payload_len < 5 {
            return Err(CcmError::InvalidMIC);
        }

        if cipher_packet.len() < payload_len + HEADER_SIZE
            || clear_packet.len() < payload_len + HEADER_SIZE - MIC_SIZE
        {
            return Err(CcmError::WrongPacketLength);
        }

        if scratch.len() < (payload_len + 16).max(MINIMUM_SCRATCH_AREA_SIZE) {
            return Err(CcmError::InsufficientScratchArea);
        }

        #[cfg(feature = "51")]
        {
            if payload_len > MAXIMUM_LENGTH_5BITS {
                return Err(CcmError::WrongPacketLength);
            }
        }

        #[cfg(not(feature = "51"))]
        let length_variant = if payload_len <= MAXIMUM_LENGTH_5BITS {
            LENGTH_A::DEFAULT
        } else {
            #[cfg(any(feature = "52840", feature = "52833", feature = "52810"))]
            // NOTE(unsafe) Any 8bits pattern is safe to write to this register
            self.regs
                .maxpacketsize
                .write(|w| unsafe { w.maxpacketsize().bits(payload_len as u8) });

            LENGTH_A::EXTENDED
        };

        #[cfg(feature = "51")]
        self.regs.mode.write(|w| w.mode().decryption());

        #[cfg(not(feature = "51"))]
        self.regs
            .mode
            .modify(|_, w| w.mode().decryption().length().variant(length_variant));

        // Setup the pointers
        // NOTE(unsafe) These addreses are in RAM, checked above
        unsafe {
            self.regs
                .cnfptr
                .write(|w| w.bits(ccm_data as *mut _ as u32));

            self.regs
                .inptr
                .write(|w| w.bits(cipher_packet.as_ptr() as u32));
            self.regs
                .outptr
                .write(|w| w.bits(clear_packet.as_mut_ptr() as u32));
            self.regs
                .scratchptr
                .write(|w| w.bits(scratch.as_mut_ptr() as u32));
        }

        // Clear events
        self.regs.events_endcrypt.reset();
        self.regs.events_error.reset();
        self.regs.events_endksgen.reset();

        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);

        // Start key generation
        // NOTE(unsafe) 1 is a valid pattern to write to this register
        self.regs.tasks_ksgen.write(|w| unsafe { w.bits(1) });

        while self.regs.events_endksgen.read().bits() == 0 {}

        // NOTE(unsafe) 1 is a valid pattern to write to this register
        self.regs.tasks_crypt.write(|w| unsafe { w.bits(1) });

        while self.regs.events_endcrypt.read().bits() == 0
            && self.regs.events_error.read().bits() == 0
        {}

        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        if self.regs.events_error.read().bits() == 1 {
            // It's ok to return here, the events will be cleared before the next
            // encryption/decryption
            return Err(CcmError::EasyDMAError);
        }

        if self.regs.micstatus.read().micstatus().is_check_failed() {
            return Err(CcmError::InvalidMIC);
        }

        ccm_data.increment_counter();
        Ok(())
    }
}
