# native-fault-v2: independent DFL-only source prototype

This source-only revision is not built, run, deployed, registered with Cargo,
or used by shared Maclator. The v1 source and failed measurements remain intact.
The six-file v2 tree intentionally contains no Cargo manifest or main/platform
integration. REP ownership validation, precise progress, registry ABI, errno,
output alias rejection, and DF rules are unchanged from frozen v1. v1's actual
host results do not establish that this revision passes.

## Installation contract

Call before guest threads, while the caller exclusively controls native
SIGSEGV/SIGBUS sigaction initialization. No concurrent native sigaction changes
are supported. BOTH dispositions are queried before validation and the first
write. ANY unknown custom callback (one-argument or SA_SIGINFO), SIG_IGN, or any
other non-DFL pointer makes installation return ENOTSUP without attempting a
write to either signal. A query error also precedes writes. The test must check
both sides independently: finding an unsupported BUS must not first overwrite
SEGV. Darwin queries on this host omit SA_RESETHAND; testing that visible bit
cannot prove that an unknown callback is persistent.

Only prior **DFL disposition behavior** is supported. The dispatcher is
canonical SA_SIGINFO with only SEGV/BUS in its extra mask. Prior DFL flags/masks
are not propagated to it. Every unmatched signal commits to canonical fatal
DFL, flags 0, empty action mask, then unblocks and raises that signal. The
callback never calls a prior custom handler and never temporarily removes a
capture action while continuing the process. Existing host diagnostic handlers
must therefore make this prototype decline installation; it is not a general
transparent sigaction replacement.

## Failure and rollback boundaries

`mcl_nf_install_with_report` returns the primary errno-style error and writes a
16-byte report: primary_error, rollback_error, changed_mask, restored_mask.
Bit 1 represents SEGV and bit 2 BUS. changed_mask describes successful writes,
not attempted writes; restored_mask describes successful rollback writes.
`mcl_nf_install` is only a convenience wrapper that discards this report.
A failed first write requires no rollback. A failed BUS write attempts exactly
one SEGV rollback from the **query-visible** DFL snapshot. Successful rollback
restores DFL termination behavior and observable flags/mask, within the
exclusive-startup precondition. It cannot restore a hidden raw RESET bit.

Do not call this exact raw-state rollback. DFL plus hidden RESET still defaults
to a fatal signal; that behavior is the narrower contract. No public-query test
can prove hidden RESET equality before/after rollback. A future runtime should
install its own authoritative DFL/flags0/empty-mask configuration and retain
that original configuration; this revision does not add an authority-tracking
API or intercept unrelated host sigaction calls.

Every installation failure must stop startup. installed is published only after
two successful writes. On rollback failure, rollback_error remains separate from
the primary error and one dispatcher may remain installed while installed=0.
No guest primitive becomes enabled. Retrying is unsupported until the caller
restores BOTH authoritative native defaults itself. This source does not hide a
rollback error, promise recovery from arbitrary kernel/libc failures, or mutate
an unknown custom action to obtain a convenient fallback.

## Fault capture scope

Exact REP forward/backward COPY and FILL labels, saved r9/r10/RSP ownership,
trap14, data-fault code/address/CR2/write bit/DF/cursor validation and normal
leaf return are inherited. Captured FAULT and INTERNAL may follow partial
writes; neither is success nor permission to rerun the original operation.
Only the remaining interval may be continued by an explicit supported caller.
The result must be writable and independent of runtime internals. Registry
capacity is bounded; foreign assembly's aligned atomic stack-cell store is a
Darwin x86-64 ABI contract, not portable ISO C atomic code.

Guest FAR/ESR/frame/token delivery, shim RESUME, tagged return values, nested or
nonlocal cleanup, VM concurrency, arbitrary native PC changes, longjmp and
pthread-exit lifecycle are not implemented. This is not deployable.

Validation source is ../host.
Only the parent may later build and run its bounded fresh-process cases serially.
No source inspection, old-v1 pass, or injected test implies measured v2 success,
application compatibility, runtime speedup, or the user's 95% target.
