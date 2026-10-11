//! Publishes captured game state to `Local\xenomit_state_<PID>` so readers
//! (`xenomit-game-api`'s `LiveStateReader`) never touch the player thread.
//!
//! Writer protocol: bump `seq` to odd, copy the whole frame, bump `seq` to
//! even — a seqlock. Readers retry while `seq` is odd or changes mid-read.

use std::sync::atomic::{AtomicU32, Ordering};

use xenomit_game_api::live::layout::{self, LiveEntityRecord, LiveHeader};

/// Creates and owns the `Local\xenomit_state_<pid>` file mapping.
#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr::NonNull;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
        PAGE_READWRITE, UnmapViewOfFile,
    };

    pub struct StateShm {
        mapping: windows_sys::Win32::Foundation::HANDLE,
        base: NonNull<u8>,
    }

    impl StateShm {
        pub fn create() -> Option<Self> {
            let name: Vec<u16> = layout::mapping_name(std::process::id())
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: name is NUL-terminated; the mapping is created fresh
            // and owned by this process.
            let mapping = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    (layout::MAPPING_LEN >> 32) as u32,
                    (layout::MAPPING_LEN & 0xFFFF_FFFF) as u32,
                    name.as_ptr(),
                )
            };
            if mapping.is_null() {
                let error = unsafe { GetLastError() };
                eprintln!("[state] CreateFileMappingW failed: {error}");
                return None;
            }
            // SAFETY: mapping is a valid handle owned by us.
            let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
            let base = NonNull::new(view.Value.cast::<u8>());
            let Some(base) = base else {
                let error = unsafe { GetLastError() };
                eprintln!("[state] MapViewOfFile failed: {error}");
                unsafe { CloseHandle(mapping) };
                return None;
            };
            // The mapping starts zero-filled; mark the header so readers see
            // magic+version even before the first frame.
            unsafe {
                std::ptr::write(
                    base.as_ptr().cast::<LiveHeader>(),
                    LiveHeader {
                        magic: layout::MAGIC,
                        version: layout::VERSION,
                        map_id: -1,
                        ..LiveHeader::default()
                    },
                );
            }
            Some(Self { mapping, base })
        }
    }

    impl super::Publisher for StateShm {
        fn publish(&mut self, header: &LiveHeader, entities: &[LiveEntityRecord], strings: &[u8]) {
            debug_assert_eq!(header.entity_count as usize, entities.len());
            debug_assert_eq!(header.string_bytes as usize, strings.len());
            let strings = &strings[..strings.len().min(layout::MAX_STRING_LEN)];
            let entities = &entities[..entities.len().min(layout::MAX_ENTITIES)];
            unsafe {
                let base = self.base.as_ptr();
                let seq = &*base.add(layout::SEQ_OFFSET).cast::<AtomicU32>();
                // Seqlock: seq goes odd, payload copy, seq goes even.
                seq.fetch_add(1, Ordering::Relaxed);
                std::sync::atomic::fence(Ordering::Release);

                let mut header = *header;
                // Keep `seq` untouched inside the payload copy.
                header.seq = seq.load(Ordering::Relaxed);
                std::ptr::write(base.cast::<LiveHeader>(), header);
                if !entities.is_empty() {
                    std::ptr::copy_nonoverlapping(
                        entities.as_ptr(),
                        base.add(layout::HEADER_LEN).cast::<LiveEntityRecord>(),
                        entities.len(),
                    );
                }
                if !strings.is_empty() {
                    std::ptr::copy_nonoverlapping(
                        strings.as_ptr(),
                        base.add(layout::STRINGS_OFFSET),
                        strings.len(),
                    );
                }

                std::sync::atomic::fence(Ordering::Release);
                seq.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    impl Drop for StateShm {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.base.as_ptr().cast(),
                });
                CloseHandle(self.mapping);
            }
        }
    }
}

/// Writer half of the seqlock; platform implementations only need this.
#[cfg(windows)]
trait Publisher {
    fn publish(&mut self, header: &LiveHeader, entities: &[LiveEntityRecord], strings: &[u8]);
}

/// Shared-memory publisher handle. On non-Windows platforms publishing is a
/// no-op (the bridge keeps working through the pipe).
pub struct LiveStatePublisher {
    #[cfg(windows)]
    imp: Option<imp::StateShm>,
}

impl LiveStatePublisher {
    /// Create the mapping for this process. `None` (and a logged error) when
    /// the OS call fails; publishing then becomes a no-op.
    #[cfg(windows)]
    pub fn new() -> Self {
        Self { imp: imp::StateShm::create() }
    }

    /// Non-Windows stub.
    #[cfg(not(windows))]
    pub fn new() -> Self {
        Self {}
    }

    /// Atomically publish one captured frame (seqlock bump-copy-bump).
    pub fn publish(&mut self, header: &LiveHeader, entities: &[LiveEntityRecord], strings: &[u8]) {
        #[cfg(windows)]
        if let Some(imp) = &mut self.imp {
            imp.publish(header, entities, strings);
        }
        #[cfg(not(windows))]
        let _ = (header, entities, strings);
    }
}
