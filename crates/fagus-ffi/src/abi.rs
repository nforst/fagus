//! The functions C calls, declared in `include/fagus.h`. This is the unsafe code of the library:
//! it reads the memory the caller passes, by pointer and length, and hands out memory that the
//! caller gives back to the function that frees it. Every function catches panics, which must
//! not unwind into C, and checks pointers for null; it cannot check that a pointer that is not
//! null points where the caller says, which the caller promises.
#![allow(
    unsafe_code,
    reason = "the boundary to C: raw pointers from the caller and exported symbols"
)]

use std::ffi::{CString, c_char};

use crate::{Rendered, guard};

/// An engine: fonts, caches and threads, for any number of documents.
pub struct FagusEngine(fagus::Engine);

/// The answer to a render request, until [`fagus_result_free`].
pub struct FagusResult(Rendered);

/// The bytes at `pointer`, `length` of them; none for a null pointer, unless `length` is 0.
///
/// # Safety
///
/// A `pointer` that is not null points to `length` bytes that stay unchanged while the slice
/// lives.
unsafe fn bytes<'a>(pointer: *const c_char, length: usize) -> Option<&'a [u8]> {
    if pointer.is_null() {
        return (length == 0).then_some(&[]);
    }
    if isize::try_from(length).is_err() {
        return None;
    }
    // SAFETY: the pointer is not null, and the caller promises `length` readable bytes there,
    // which `from_raw_parts` needs together with a length of at most `isize::MAX`.
    Some(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) })
}

/// Writes `length` to `out`, unless it is null.
///
/// # Safety
///
/// A `out` that is not null points to a writable `size_t`.
unsafe fn put(out: *mut usize, length: usize) {
    if !out.is_null() {
        // SAFETY: the caller promises that a pointer that is not null may be written.
        unsafe { out.write(length) };
    }
}

/// The version of Fagus, such as `1.0.0`: a string that lives as long as the library.
#[unsafe(no_mangle)]
pub extern "C" fn fagus_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The version of the C interface: 0 while it is experimental.
#[unsafe(no_mangle)]
pub extern "C" fn fagus_abi() -> u32 {
    crate::ABI
}

/// The worker protocol whose render requests the library takes (`protocol` in its `H`).
#[unsafe(no_mangle)]
pub extern "C" fn fagus_protocol() -> u32 {
    1
}

/// A new engine with the options of `options` (JSON, `options_len` bytes; null or empty for the
/// defaults). On failure it returns null and, if `error` is not null, writes a message there to
/// give back with [`fagus_string_free`].
///
/// # Safety
///
/// `options` is null or points to `options_len` bytes; `error` is null or points to a writable
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_engine_new(
    options: *const c_char,
    options_len: usize,
    error: *mut *mut c_char,
) -> *mut FagusEngine {
    let created = guard(
        || {
            // SAFETY: as the caller promises.
            let options = unsafe { bytes(options, options_len) }.ok_or("the options are a null pointer")?;
            crate::engine(options)
        },
        |message| Err(message.to_owned()),
    );
    match created {
        Ok(engine) => Box::into_raw(Box::new(FagusEngine(engine))),
        Err(message) => {
            if !error.is_null() {
                let message = CString::new(message.replace('\0', " ")).unwrap_or_default();
                // SAFETY: the caller promises that a pointer that is not null may be written.
                unsafe { error.write(message.into_raw()) };
            }
            std::ptr::null_mut()
        }
    }
}

/// Ends an engine. Null is ignored.
///
/// # Safety
///
/// `engine` is null or comes from [`fagus_engine_new`], has not been freed yet and renders
/// nothing at the time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_engine_free(engine: *mut FagusEngine) {
    if !engine.is_null() {
        // SAFETY: the engine came from `Box::into_raw` in `fagus_engine_new` and is freed once.
        let engine = unsafe { Box::from_raw(engine) };
        guard(|| drop(engine), |_| ());
    }
}

/// Renders a document: `request` is the JSON header of a render request of the worker protocol
/// (`R`, `request_len` bytes), and `data` (`data_len` bytes) its data blocks one after another,
/// as long as the header's `blobs` say. The answer is never null: either a PDF or an error, with
/// the header of `P` or `E` as JSON; give it back with [`fagus_result_free`]. An engine may
/// render documents on several threads at once.
///
/// # Safety
///
/// `engine` is null or a live engine of [`fagus_engine_new`]; `request` and `data` are null or
/// point to as many bytes as their lengths say.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_render(
    engine: *const FagusEngine,
    request: *const c_char,
    request_len: usize,
    data: *const c_char,
    data_len: usize,
) -> *mut FagusResult {
    let rendered = guard(
        || {
            // SAFETY: the caller promises a live engine or null.
            let Some(FagusEngine(engine)) = (unsafe { engine.as_ref() }) else {
                return Rendered::error("the engine is a null pointer");
            };
            // SAFETY: as the caller promises.
            let (Some(request), Some(data)) = (unsafe { bytes(request, request_len) }, unsafe {
                bytes(data, data_len)
            }) else {
                return Rendered::error("the request or its data is a null pointer");
            };
            if request.is_empty() {
                return Rendered::error("the request is empty");
            }
            crate::render(engine, request, data)
        },
        Rendered::error,
    );
    Box::into_raw(Box::new(FagusResult(rendered)))
}

/// Whether the answer is a PDF (1) or an error (0).
///
/// # Safety
///
/// `result` is null or a live answer of [`fagus_render`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_result_ok(result: *const FagusResult) -> i32 {
    // SAFETY: the caller promises a live answer or null.
    i32::from(unsafe { result.as_ref() }.is_some_and(|FagusResult(rendered)| rendered.ok))
}

/// The header of the answer, JSON ending with NUL, and its length without the NUL in `len` if
/// that is not null. It lives as long as the answer.
///
/// # Safety
///
/// `result` is null or a live answer of [`fagus_render`]; `len` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_result_header(result: *const FagusResult, len: *mut usize) -> *const c_char {
    // SAFETY: the caller promises a live answer or null.
    let Some(FagusResult(rendered)) = (unsafe { result.as_ref() }) else {
        // SAFETY: as the caller promises.
        unsafe { put(len, 0) };
        return std::ptr::null();
    };
    // SAFETY: as the caller promises.
    unsafe { put(len, rendered.header.as_bytes().len()) };
    rendered.header.as_ptr()
}

/// The PDF of the answer, and its length in `len` if that is not null; null for an error. It
/// lives as long as the answer.
///
/// # Safety
///
/// `result` is null or a live answer of [`fagus_render`]; `len` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_result_pdf(result: *const FagusResult, len: *mut usize) -> *const u8 {
    // SAFETY: the caller promises a live answer or null.
    match unsafe { result.as_ref() } {
        Some(FagusResult(rendered)) if rendered.ok => {
            // SAFETY: as the caller promises.
            unsafe { put(len, rendered.pdf.len()) };
            rendered.pdf.as_ptr()
        }
        _ => {
            // SAFETY: as the caller promises.
            unsafe { put(len, 0) };
            std::ptr::null()
        }
    }
}

/// Gives an answer back. Null is ignored.
///
/// # Safety
///
/// `result` is null or comes from [`fagus_render`] and has not been freed yet; nothing it handed
/// out is used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_result_free(result: *mut FagusResult) {
    if !result.is_null() {
        // SAFETY: the answer came from `Box::into_raw` in `fagus_render` and is freed once.
        drop(unsafe { Box::from_raw(result) });
    }
}

/// Gives a message of [`fagus_engine_new`] back. Null is ignored.
///
/// # Safety
///
/// `string` is null or a message of this library that has not been freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fagus_string_free(string: *mut c_char) {
    if !string.is_null() {
        // SAFETY: the message came from `CString::into_raw` and is freed once.
        drop(unsafe { CString::from_raw(string) });
    }
}
