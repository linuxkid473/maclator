.text
.p2align 2
.globl _maclator_branch_loop
_maclator_branch_loop:
    mov x2, #0
1:
    add x1, x1, #37
    tbz x1, #0, 2f
    add x2, x2, #1
2:
    tbnz x1, #8, 3f
    add x2, x2, #3
3:
    lsr x3, x1, #6
    and x3, x3, #1
    cbz w3, 4f
    add x2, x2, #5
4:
    cbnz x3, 5f
    add x2, x2, #7
5:
    sub x0, x0, #1
    cbnz x0, 1b
    mov x0, x2
    ret
