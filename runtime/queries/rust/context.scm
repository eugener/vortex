; Structural scopes for the sticky context header (SPEC §7.5, M8). Each
; `@context` capture names a node whose *first line* may be pinned at the top of
; the viewport while its body is scrolled through.
;
; Only nodes whose first line says something a reader has lost: what item they
; are inside. Blocks that carry no identity of their own - a bare `{ ... }`, a
; `match` arm's body, a closure - are deliberately absent, since pinning them
; spends a row to say "you are inside a block" while pushing out the row that
; says which function.

(mod_item) @context
(struct_item) @context
(enum_item) @context
(union_item) @context
(trait_item) @context
(impl_item) @context
(function_item) @context
(function_signature_item) @context
(macro_definition) @context

; Control flow deep inside a function: which branch you are reading is the
; question the top of the screen stops answering once the condition scrolls off.
(if_expression) @context
(else_clause) @context
(match_expression) @context
(for_expression) @context
(while_expression) @context
(loop_expression) @context
