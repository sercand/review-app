# review-sandbox

Copied from `otsimo/automated-qa-framework` (`qa-sandbox`, local half) by its author; MIT-licensed here.

Local sandbox ops confined to one project root: file read/write/list/exists, `glob`
and `grep` (gitignore-aware, capped) over a canonical [`Root`], and a persistent
`bash` driven on a PTY (`BashSession`, Unix only) behind the `SandboxOps` trait.
Every caller-supplied path is resolved against the root and refused when it would
escape it. The OS-level sandbox arrives later.
