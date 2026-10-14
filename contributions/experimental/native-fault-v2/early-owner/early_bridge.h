#ifndef MCL_EARLY_BRIDGE_H
#define MCL_EARLY_BRIDGE_H
#include <stdint.h>
#include "../native-fault/native_fault.h"
struct mcl_eo_action { uint64_t handler; uint32_t flags, mask; };
struct mcl_eo_altstack { uint64_t pointer, size; int32_t flags, query_error; };
struct mcl_eo_record {
    uint32_t scenario, constructor_entered, constructor_finished, main_seen;
    int32_t original_query_error, fixture_error, installer_return;
    uint32_t install_attempted, decision, native_queries, native_writes;
    int32_t after_query_error, post_fixture_error;
    uint32_t post_fixture_applied, hidden_reset_requested, native_installed;
    struct mcl_nf_install_report report;
    struct mcl_eo_action original[2], before_install[2], after_install[2], at_ctor_exit[2];
    struct mcl_eo_altstack original_altstack;
    uint32_t sequence_count, sequence[8];
};
enum { MCL_EO_BASELINE=0, MCL_EO_NATURAL=1, MCL_EO_CUSTOM_SEGV=2,
       MCL_EO_CUSTOM_BUS=3, MCL_EO_IGNORE_SEGV=4, MCL_EO_IGNORE_BUS=5,
       MCL_EO_LATE_CUSTOM=6, MCL_EO_LATE_DEFAULT=7, MCL_EO_LATE_RESET=8 };
enum { MCL_EO_OBSERVE=0, MCL_EO_INSTALLED=1, MCL_EO_UNSUPPORTED=2 };
void mcl_eo_record_at_main(struct mcl_eo_record *);
int mcl_eo_current(struct mcl_eo_action *, struct mcl_eo_altstack *);
int mcl_eo_visible_recipe_matches(const struct mcl_eo_action *);
void mcl_eo_idempotent_report(struct mcl_nf_install_report *, uint32_t *);
uint32_t mcl_eo_reset_flag(void);
uint32_t mcl_eo_onstack_flag(void);
uint32_t mcl_eo_siginfo_flag(void);
int32_t mcl_eo_altstack_disabled_flag(void);
int32_t mcl_eo_enotsup(void);
#endif
