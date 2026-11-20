/*
 * The C library of Fagus: HTML and CSS to PDF in the caller's process.
 *
 * EXPERIMENTAL: this interface is not covered by the stability promise of Fagus 1.x and may
 * change in any release; fagus_abi() returns 0 while that is so. See docs/ffi.md.
 *
 * A document is the render request of the worker protocol (docs/protocol.md): the JSON header
 * of `R` and its data blocks one after another. The answer is the JSON header of `P` or `E`
 * and the PDF. The library asks nothing back: a request with `ask`, or a signature with
 * `certificates`, `timestamp` or `revocation`, is answered with an error.
 *
 * Kept in step with crates/fagus-ffi/src/abi.rs by a test of the crate.
 */

#ifndef FAGUS_H
#define FAGUS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* An engine: fonts, caches and threads, for any number of documents. */
typedef struct FagusEngine FagusEngine;

/* The answer to a render request. */
typedef struct FagusResult FagusResult;

/* The version of Fagus, such as "1.0.0"; lives as long as the library. */
const char *fagus_version(void);

/* The version of this interface: 0 while it is experimental. */
uint32_t fagus_abi(void);

/* The worker protocol whose render requests the library takes. */
uint32_t fagus_protocol(void);

/*
 * A new engine with the options of `options` (JSON, `options_len` bytes; NULL or empty for the
 * defaults): fontDirs, systemFonts, locale, sansSerif, serif, monospace, memoryBudget (bytes),
 * timeLimit (milliseconds). On failure it returns NULL and, if `error` is not NULL, writes a
 * message there to give back with fagus_string_free().
 */
FagusEngine *fagus_engine_new(const char *options, size_t options_len, char **error);

/* Ends an engine that renders nothing at the time. NULL is ignored. */
void fagus_engine_free(FagusEngine *engine);

/*
 * Renders a document: `request` is the JSON header of a render request (`request_len` bytes),
 * `data` its data blocks one after another (`data_len` bytes), as long as the header's `blobs`
 * say. Never returns NULL; give the answer back with fagus_result_free(). An engine may render
 * documents on several threads at once.
 */
FagusResult *fagus_render(const FagusEngine *engine, const char *request, size_t request_len,
                          const char *data, size_t data_len);

/* Whether the answer is a PDF (1) or an error (0). */
int32_t fagus_result_ok(const FagusResult *result);

/*
 * The header of the answer (`P` or `E`), JSON ending with NUL; its length without the NUL goes
 * to `len` if that is not NULL. Lives as long as the answer.
 */
const char *fagus_result_header(const FagusResult *result, size_t *len);

/* The PDF of the answer and its length, or NULL for an error. Lives as long as the answer. */
const uint8_t *fagus_result_pdf(const FagusResult *result, size_t *len);

/* Gives an answer back. NULL is ignored. */
void fagus_result_free(FagusResult *result);

/* Gives a message of fagus_engine_new() back. NULL is ignored. */
void fagus_string_free(char *string);

#ifdef __cplusplus
}
#endif

#endif /* FAGUS_H */
