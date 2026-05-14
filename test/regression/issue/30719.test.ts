// https://github.com/oven-sh/bun/issues/30719
//
// `bun_core::PathString::init` packs a `&[u8]`'s (ptr, len) into the
// backing int and returns a `Copy + 'static` value — the backing slice's
// lifetime is erased. With `init` declared as a safe `fn`, entirely safe
// Rust could construct a `PathString` whose `slice()` read freed memory:
//
//   let test = Box::new(*b"Hello World");
//   let init = PathString::init(&*test);
//   drop(test);
//   init.slice();  // UB — dangling &[u8]
//
// The fix makes `init` an `unsafe fn` with a documented outlives
// contract. This test asserts the signature stays `unsafe`: without the
// fix applied, the signature is plain `pub fn init`, which means any
// future revert (or accidental un-unsafe-ing) will trip this test
// before review.
//
// Runtime assertion (rather than a JS-observable behavior check)
// because the bug is an API-surface soundness hole, not a reachable
// crash from safe call sites in the tree — every in-tree caller was
// already sound. The regression guarded here is "someone removes
// `unsafe` from `PathString::init` and reopens the hole".

import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

test("PathString::init is declared unsafe (soundness invariant for #30719)", () => {
  // Resolve relative to the repo root — this test file lives at
  // test/regression/issue/30719.test.ts, the source at
  // src/bun_core/string/PathString.rs. Walk up two directories from
  // test/regression/issue/ to reach the repo root.
  const repoRoot = join(import.meta.dir, "..", "..", "..");
  const src = readFileSync(join(repoRoot, "src/bun_core/string/PathString.rs"), "utf8");

  // Normalize whitespace so a reformat doesn't spuriously break this.
  const normalized = src.replace(/\s+/g, " ");

  // Positive: unsafe signature is present.
  expect(normalized).toContain("pub unsafe fn init(str: &[u8]) -> Self");

  // Negative: the plain-safe signature (the bug) is NOT present. If
  // someone drops the `unsafe` keyword, this assertion fires.
  expect(normalized).not.toContain("pub fn init(str: &[u8]) -> Self");
});
