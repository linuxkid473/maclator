#ifndef MCL_NATIVE_FAULT_H
#define MCL_NATIVE_FAULT_H

// Source prototype only. No constructor and no automatic runtime installation.
// Only Darwin x86-64 data faults at our labelled REP instructions are captured.
#include <stddef.h>
#include <stdint.h>

enum { MCL_NF_COPY = 1, MCL_NF_FILL = 2 };
enum { MCL_NF_OK = 0, MCL_NF_FAULT = 1, MCL_NF_DECLINED = -1,
       MCL_NF_REENTRANT = -2, MCL_NF_INTERNAL = -3 };

struct mcl_nf_result {
    uint64_t destination_next;
    uint64_t source_next;
    uint64_t remaining;
    uint64_t completed;
    uint64_t host_address;
    uint64_t host_pc;
    uint64_t host_rflags;
    int32_t signal_number;
    int32_t signal_code;
    int32_t writing;
    int32_t backward;
    uint32_t exception_error;
    uint32_t trap_number;
    uint32_t original_mask;
    uint32_t reserved;
};

enum { MCL_NF_SIGNAL_SEGV = 1, MCL_NF_SIGNAL_BUS = 2 };
struct mcl_nf_install_report {
    int32_t primary_error;  // Returned errno-style code; 0 means installed.
    int32_t rollback_error; // Nonzero means rollback failed; abort startup.
    uint32_t changed_mask;  // Successful dispatcher writes in this attempt.
    uint32_t restored_mask; // Successful rollback writes in this attempt.
};

// Startup-only before guest threads, with no concurrent native sigaction
// mutation. Both dispositions must be DFL; ANY custom/IGN returns ENOTSUP
// after two queries and before any disposition write. Query failures also
// precede writes. Unmatched signals use canonical DFL/flags0/empty-mask fatal
// behavior. Successful dispatcher installation canonicalizes prior DFL flags
// and masks; it does not preserve ONSTACK/RESTART/NODEFER customization.
// On failed second write, rollback restores DFL behavior and query-visible
// mask/flags only. Darwin may hide SA_RESETHAND: exact raw-state rollback is
// impossible from these snapshots. Future runtime integration should own an
// authoritative canonical DFL/flags0/empty-mask initialization configuration.
// Any failure must stop startup. report, if non-NULL, must be writable; callers
// needing rollback diagnosis use this form and inspect rollback_error/masks.
// Retry after rollback failure is unsupported until the caller restores both
// authoritative default dispositions itself. Later native mutations unsupported.
int mcl_nf_install_with_report(struct mcl_nf_install_report *report);
int mcl_nf_install(void); // Convenience wrapper; same limitations.

// Initialize this thread's Darwin TLV while not in a signal callback. The
// callback uses the context pointer held in saved r9, not lazy TLS lookup.
void mcl_nf_enter_thread(void);

// Inputs are normalized host pointers. Full guest mapping/commpage checks are
// the caller's responsibility; this API also rejects arithmetic/noncanonical
// intervals. No VM permissions precheck is used to promise safety.
// errno is preserved on every return. Captured FAULT is not library success:
// the caller must deliver a synchronous guest signal or terminate explicitly.
// result must be a valid writable record separate from runtime internal state.
// Nonempty guest intervals overlapping result decline without clearing it;
// declined/reentrant/internal outcomes do not provide a usable result record.
// INTERNAL may follow partial writes; never retry/fallback the original call.
// Native handlers rewriting PC or escaping nonlocally are not supported.
int mcl_nf_call(uint32_t operation, uintptr_t destination, uintptr_t source,
                size_t length, unsigned value, struct mcl_nf_result *result);

#endif
