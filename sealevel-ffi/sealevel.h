/*
 * libsealevel is a C interface for the Sealevel virtual machine.
 * This version of the library bundles the interpreter and JIT implementations part of the Rust implementation of the Solana blockchain.
 *
 * Source code: https://github.com/solana-labs/solana
 *
 * ABI stability is planned, though this version makes no promises yet.
*/

#pragma once

/* Generated with cbindgen:0.23.0 */

/* Warning, this file is autogenerated by cbindgen. Don't modify this manually. */

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#define SEALEVEL_OK 0

#define SEALEVEL_ERR_INVALID_ELF 1

#define SEALEVEL_ERR_SYSCALL_REGISTRATION 2

#define SEALEVEL_ERR_CALL_DEPTH_EXCEEDED 3

#define SEALEVEL_ERR_UNKNOWN -1

/**
 * The invoke context holds the state of a single transaction execution.
 * It tracks the execution progress (instruction being executed),
 * interfaces with account data,
 * and specifies the on-chain execution rules (precompiles, syscalls, sysvars).
 */
typedef struct sealevel_invoke_context sealevel_invoke_context;

/**
 * A virtual machine capable of executing Solana Sealevel programs.
 */
typedef struct sealevel_machine sealevel_machine;

/**
 * A virtual machine program ready to be executed.
 */
typedef struct sealevel_program sealevel_program;

/**
 * Access parameters of an account usage in an instruction.
 */
typedef struct {
  size_t index_in_transaction;
  size_t index_in_caller;
  bool is_signer;
  bool is_writable;
} sealevel_instruction_account;

/**
 * The map of syscalls provided by the virtual machine.
 */
typedef SyscallRegistry *sealevel_syscall_registry;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Returns the error code of this thread's last seen error.
 */
int sealevel_errno(void);

/**
 * Returns a UTF-8 string of this thread's last seen error,
 * or NULL if `sealevel_errno() == SEALEVEL_OK`.
 *
 * Must be released using `sealevel_strerror_free` after use.
 */
const char *sealevel_strerror(void);

/**
 * Frees an unused error string gained from `sealevel_strerror`.
 * Calling this with a NULL pointer is a no-op.
 */
void sealevel_strerror_free(const char *str);

/**
 * Creates a new Sealevel machine environment.
 */
sealevel_machine *sealevel_machine_new(void);

/**
 * Releases resources associated with a Sealevel machine.
 */
void sealevel_machine_free(sealevel_machine *machine);

/**
 * Drops an invoke context and all programs created with it.
 */
void sealevel_invoke_context_free(sealevel_invoke_context *this_);

/**
 * Processes a transaction instruction.
 *
 * Sets `sealevel_errno`.
 */
void sealevel_process_instruction(sealevel_invoke_context *invoke_context,
                                  const char *data,
                                  size_t data_len,
                                  const sealevel_instruction_account *accounts,
                                  size_t accounts_len,
                                  uint64_t *compute_units_consumed);

/**
 * Loads a Sealevel program from an ELF buffer and verifies its SBF bytecode.
 *
 * Consumes the given syscall registry.
 */
sealevel_program *sealevel_program_create(const sealevel_machine *machine,
                                          sealevel_syscall_registry syscalls,
                                          const char *data,
                                          size_t data_len);

/**
 * Compiles a program to native executable code.
 *
 * Sets `sealevel_errno`.
 */
void sealevel_program_jit_compile(sealevel_program *program);

/**
 * Executes a Sealevel program with the given instruction data and accounts.
 *
 * Unlike `sealevel_process_instruction`, does not progress the transaction context state machine.
 */
uint64_t sealevel_program_execute(const sealevel_program *program,
                                  const sealevel_invoke_context *invoke_context,
                                  const char *data,
                                  size_t data_len,
                                  const sealevel_instruction_account *accounts,
                                  size_t accounts_len);

#ifdef __cplusplus
} // extern "C"
#endif // __cplusplus
