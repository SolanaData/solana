import {Connection} from '../connection';
import {Transaction} from '../transaction';
import type {ConfirmOptions} from '../connection';
import type {TransactionSignature, TransactionSigner} from '../transaction';

/**
 * Sign, send and confirm a transaction.
 *
 * If `commitment` option is not specified, defaults to 'max' commitment.
 *
 * @param {Connection} connection
 * @param {Transaction} transaction
 * @param {Array<TransactionSigner>} signers
 * @param {ConfirmOptions} [options]
 * @returns {Promise<TransactionSignature>}
 */
export async function sendAndConfirmTransaction(
  connection: Connection,
  transaction: Transaction,
  signers: Array<TransactionSigner>,
  options?: ConfirmOptions,
): Promise<TransactionSignature> {
  const sendOptions = options && {
    skipPreflight: options.skipPreflight,
    preflightCommitment: options.preflightCommitment || options.commitment,
  };

  const signature = await connection.sendTransaction(
    transaction,
    signers,
    sendOptions,
  );

  const status = (
    await connection.confirmTransaction(
      signature,
      options && options.commitment,
    )
  ).value;

  if (status.err) {
    throw new Error(
      `Transaction ${signature} failed (${JSON.stringify(status)})`,
    );
  }

  return signature;
}
