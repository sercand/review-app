# Notice

Parts of this crate are adapted from [amenocturne/da](https://github.com/amenocturne/da) (MIT).

```text
MIT License

Copyright (c) 2026 amenocturne

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Corpus provenance


The `.tests` files in this directory are vendored from the
[Parable](https://github.com/ldayton/Parable) bash-parser test suite,
obtained via [rable](https://github.com/mpecan/rable) at commit
[`411dd22`](https://github.com/mpecan/rable/tree/411dd22) (rable 0.2.0,
2026-04-19). Both upstream projects are MIT-licensed.

### Files vendored

In-scope (the parser must accept and classify):

- `01_words.tests` — word tokenization, quotes, escapes
- `02_commands.tests` — simple commands, argv
- `03_pipelines.tests` — pipe operators
- `04_lists.tests` — `&&`, `||`, `;`
- `05_redirects.tests` — all redirect forms
- `16_negation_time.tests` — `!` and `time` prefixes
- `22_pipe_stderr.tests` — `|&`
- `24_ansi_c_quoting.tests` — `$'...'`
- `34_line_continuation.tests` — `\<newline>`

Bail-expected (the parser must correctly refuse):

- `12_command_substitution.tests` — `$(...)`, backticks
- `13_arithmetic.tests` — `$((...))`
- `14_here_documents.tests` — `<<EOF`, `<<<`
- `15_process_substitution.tests` — `<(...)`, `>(...)`

### Upstream licenses

- Parable — MIT, Copyright © Logan Dayton
- rable — MIT, Copyright © 2025 Matjaz Domen Pecan

The differential harness at `src/shparse/corpus_tests.rs` consumes these
files verbatim; any modifications should be upstream contributions.
