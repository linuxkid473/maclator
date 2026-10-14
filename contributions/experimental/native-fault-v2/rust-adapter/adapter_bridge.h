#ifndef MCL_RA_BRIDGE_H
#define MCL_RA_BRIDGE_H
#include <stddef.h>
#include <stdint.h>
#include "../native-fault/native_fault.h"
struct mcl_ra_state {
    uint32_t armed, active, reserved, local_registered, nonzero_stack_cells;
};
struct mcl_ra_action { uint64_t handler; int32_t flags; uint32_t mask; };
struct mcl_ra_fixture {
    uint64_t destination, source, length, completed, fault_address, page_size;
    uint32_t operation, value, backward, writing;
};
size_t mcl_ra_layout(uint32_t *, size_t);
void mcl_ra_state_snapshot(struct mcl_ra_state *);
void mcl_ra_set_armed(uint32_t);
int mcl_ra_prepare_defaults(void);
int mcl_ra_prepare_custom(void);
int mcl_ra_actions(struct mcl_ra_action *);
void mcl_ra_sigaction_counts_reset(void);
void mcl_ra_sigaction_counts(uint32_t *);
int mcl_ra_get_errno(void);
void mcl_ra_set_errno(int);
int mcl_ra_edom(void);
int mcl_ra_enotsup(void);
uint32_t mcl_ra_mask(void);
int mcl_ra_set_mask(uint32_t);
uint32_t mcl_ra_usr1_mask(void);
uint32_t mcl_ra_segv_mask(void);
int mcl_ra_observation_mode(void);
void mcl_ra_observation_reset(void);
uint64_t mcl_ra_observed_flags(void);
int mcl_ra_observed_status(void);
uint32_t mcl_ra_observed_calls(void);
uint64_t mcl_ra_read_flags(void);
int mcl_ra_fixture_setup(uint32_t, struct mcl_ra_fixture *);
int mcl_ra_fixture_unprotect(void);
int mcl_ra_fixture_matches(uint32_t);
void mcl_ra_fixture_close(void);
#endif
