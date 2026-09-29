// Wire format of the Maclator Metal bridge fast path (shared by mclmetal.m and mclbridge.m).
//
// A batch (op 4) is:   MclMsgHdr, then records. Every record starts with a 32-byte header and is a
// multiple of 8 bytes long; arguments follow the header. Little endian on both sides.
//
//   word0  len (u32) | kind (u8) << 32 | nargs (u8) << 40 | flags (u16) << 48
//   word1  selector id (u32) | aux (u32) << 32
//   word2  target handle
//   word3  result handle (MCL_R_CREATE: handle chosen by the guest)
//
// Argument: 8-byte header (tag | len << 32), then `len` payload bytes padded to 8.
// Selector ids are defined with op 5 (name) before their first use; they are process wide.
#ifndef MCLPROTO_H
#define MCLPROTO_H
#include <stdint.h>

#define MCL_MAGIC 0x424c434du   // 'MCLB'

enum { MCL_OP_RPC = 1, MCL_OP_FREE = 2, MCL_OP_EVENT = 3, MCL_OP_BATCH = 4, MCL_OP_DEFSEL = 5 };

typedef struct { uint32_t magic, cmdbytes, flags, pad; } MclMsgHdr;

enum {
    MCL_R_MSG = 1,     // fire and forget (void result)
    MCL_R_CREATE = 2,  // fire and forget, result object gets the guest-chosen handle in word3
    MCL_R_SYNC = 3,    // last record of the message; the host replies with MclReply
    MCL_R_DROP = 4,    // release one reference of `target`
};

enum {
    MCL_A_NIL = 1,     // nil object / NULL pointer
    MCL_A_HANDLE = 2,  // 8 bytes: object handle
    MCL_A_SCALAR = 3,  // raw bytes of a scalar / struct argument
    MCL_A_IDARRAY = 4, // 8 bytes per element: handles (0 = nil)
    MCL_A_DATA = 5,    // raw bytes; the argument is a pointer to a copy
    MCL_A_PLIST = 6,   // binary plist of an encoded object graph (descriptors, arrays, ...)
    MCL_A_STRING = 7,  // utf8 NSString
    MCL_A_DESC = 8,    // binary descriptor (see desc_emit in mclmetal.m)
    MCL_A_BLOCK = 9,   // completion handler: u64 block id, u8 n, n type chars of the block arguments
};

enum {
    MCL_T_VOID = 0,
    MCL_T_SCALAR = 1,  // value: raw bits
    MCL_T_NIL = 2,
    MCL_T_HANDLE = 3,  // value: handle, cls: class id; blob: plist {c,p} the first time the class is mentioned
    MCL_T_PLIST = 4,   // blob: plist
    MCL_T_EXC = 5,     // blob: utf8 message
};

typedef struct { uint32_t tag, cls; uint64_t value; uint32_t blen, pad; } MclReply;   // blob follows

#define MCL_GUEST_HANDLE_BASE (1ull << 40)

#endif
