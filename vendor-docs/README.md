# vendor-docs/ — third-party manuals (deliberately NOT in git)

This directory is **gitignored on purpose**. It exists so vendor documentation
can be dropped here for local reference without ever being committed.

**Do not commit vendor documentation to this repository.** The PocketStage repo
is public, and M-Audio's manuals are copyrighted material we do not have rights
to redistribute — format-shifting a PDF into `.txt`/`.md` does not create a
license, it just makes the copy harder to spot. A takedown doesn't remove one
file, it disables the whole repository.

## What we reference

| Document | Version | Used for |
|---|---|---|
| Oxygen Series MKV — User Guide | v1.2 | mode behavior (Preset vs DAW), `‹`/`›` semantics (§10 → D30), control inventory, personality switching combo |
| Oxygen 49 — Quickstart Guide | v1.1 | button layout / first-contact setup cross-checks |

Both come from M-Audio's support site (`m-audio.com/support`) — search the
product name and open *Manuals & Documentation*. They are never mirrored here.

## Fetching it locally (once per machine)

Download the PDFs, then extract to greppable text in this directory:

```bash
pdftotext -layout "Oxygen Series MKV - User Guide - v1.2.pdf"  vendor-docs/mkv-guide-v1.2.txt
pdftotext -layout "Oxygen 49 - Quickstart Guide - v1.1.pdf"    vendor-docs/quickstart-v1.1.txt
```

`-layout` matters: the control/CC tables reflow into readable columns instead
of a word-per-line soup.

## The rule this enforces

Ground truth about the hardware lives in **our own** measured document,
`specs/device-maps/oxygen-49-mkv.md` — measurements first, with short attributed
quotations from the guide where they settle a question (e.g. D30's §10 quote on
`‹`/`›` being mode-conditional). That is the artifact to update when a probe
teaches us something. The vendor PDF is evidence we cite, not content we host:
keep excerpts brief, always name the document and version, and never paste
long passages into repo files.
