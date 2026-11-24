# 9. Resources in batches, fetching stays with the caller

- Status: accepted
- Date: 2026-11-25

## Context

Fagus loads nothing on its own. A resource is a `data:` URL, data handed over, a file inside the
root, or what the caller's resolver answers (in Rust a `ResourceLoader`, in the worker protocol
the answer `r` to the question `?`). The engine asked for one URL, waited, and asked for the
next. For a resolver that reads from a database or a disk that is fine. For one that fetches
from the network it adds the waiting up: a catalog with 50 product photos on a CDN that answers
in 20 ms waits a second, and one with 500 waits ten, while the same requests sent together are
done in the time of the slowest. A browser loads them side by side.

Two ways lead to the same speed. The engine could fetch itself, with an HTTP client, an
allowlist and a pool of connections. Or the engine could say earlier what it needs, and the
caller, who fetches already, fetches it concurrently.

Against the first:

- The engine without network access is a property users rely on and the docs promise
  ([resources.md](../resources.md)). On Linux the worker gives the network up for good (Landlock,
  seccomp), so no document, however hostile, makes it open a connection.
- Fetching for a document that may come from user input is where server-side request forgery
  happens. What may be fetched (which hosts, which credentials, through which proxy, with which
  timeout, how large) differs with every application, and each of those would become an option
  of Fagus, with its defaults and its mistakes to answer for.
- An HTTP client with TLS in the binary is a large dependency with a steady flow of security
  fixes, for something PHP and Laravel bring along.

The engine does know most of its resources before it uses them: the `<link>`ed style sheets
after parsing, the fonts once the style sheets are read, the images once the styles are computed.
Only a few show while others are read.

## Decision

- **The engine asks for resources in batches.** `ResourceLoader` gets `load_many(&[&str])`, with
  a default that calls `load` for one after the other. A render asks with it three times: for the
  linked style sheets, for the first font file of each `@font-face` rule, and for the images, at
  most 64 URLs at a time.
- **A batch asks for nothing that would not be asked for anyway**, in the order it would be asked
  for: no candidates of a `srcset` that are not chosen, no later sources of a font, nothing behind
  a `local()` that is found. The engine keeps the answers by URL and then loads as before, one
  resource after the other, from what it has. So the PDF is the same bytes whether a loader
  answers in batches, one by one, or the data was handed over, and a loader without `load_many`
  sees what it saw before.
- **The worker protocol stays protocol 1.** A caller that can answer many at once says `askMany`
  in `R`; the worker then asks with `Q` (`urls`) and reads `q` (`found` for each, and the data of
  those found). Both sides ignore what they do not know, so every pair of old and new worker and
  caller works without a check of versions: an old worker ignores `askMany` and asks with `?`, a
  new one never sends `Q` to a caller that did not ask for it.
- **PHP gets `resolveMany()`**, a callback from a list of URLs to data by URL, beside
  `resolver()`; the one called last is asked. The package sends `askMany` whenever a resolver is
  set, so a `resolver()` saves the round trips too.
- **Fetching stays with the caller.** There is no HTTP client in the engine or in the PHP
  package, and no network access. The documentation has callbacks to copy: an allowlist of hosts,
  `Http::pool()`, `curl_multi`.
- **A cache is a hook, not a store.** `cache()` takes a PSR-16 cache of the application and puts
  it in front of the resolver. The package depends on `psr/simple-cache` only as a suggestion.

Left for later: batches for the levels of `@import`, for the sprites and images of inline SVG,
and for the documents of `<iframe>`s, which are asked for one by one as before.

## Consequences

A document with many remote images renders in about the time of its slowest image, if the
callback fetches concurrently; with `resolver()` nothing changes but the number of messages.
The rules for what is fetched stay in the application, where its hosts and secrets are, and the
sandbox of the worker stays as tight as it was.

A callback to copy is easier to get wrong than a class to call. The recipes say what each line
is for, and the allowlist is the first of them. If the same mistakes keep being made, a package
of its own with a checked fetcher is the answer, not a client in this one.

A batch is in memory at once: in the caller, in one message, and in the worker before the
memory budget is charged. Sixty-four URLs a batch bound that, and the budget and the time limit
are looked at after each.

The order in which a resolver is asked changed in one place: a later source of a font is asked
for after the first sources of all rules, not between them.
