.text
.globl _register_roundtrip
.p2align 2
_register_roundtrip:
    sub sp, sp, #64
    stp x19, x20, [sp]
    stp q8, q9, [sp, #16]
    mov x2, x1
    mov x19, #0x1234
    mov x20, #0x5678
    mov x8, #0x1122
    fmov d8, x8
    mov x8, #0x3344
    mov v8.d[1], x8
    mov x1, #30
    mov x16, #37
    svc #0x80
    stp x19, x20, [x2]
    str q8, [x2, #16]
    ldp x19, x20, [sp]
    ldp q8, q9, [sp, #16]
    add sp, sp, #64
    mov x0, #0
    ret
