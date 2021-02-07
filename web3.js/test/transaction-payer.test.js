// @flow
import {
  Account,
  Connection,
  Transaction,
  SystemProgram,
  LAMPORTS_PER_SAFE,
} from '../src';
import {mockRpc, mockRpcEnabled} from './__mocks__/node-fetch';
import {mockGetRecentBlockhash} from './mockrpc/get-recent-blockhash';
import {url} from './url';
import {mockConfirmTransaction} from './mockrpc/confirm-transaction';

if (!mockRpcEnabled) {
  // The default of 5 seconds is too slow for live testing sometimes
  jest.setTimeout(30000);
}

test('transaction-payer', async () => {
  const accountPayer = new Account();
  const accountFrom = new Account();
  const accountTo = new Account();
  const connection = new Connection(url, 'singleGossip');

  mockRpc.push([
    url,
    {
      method: 'getMinimumBalanceForRentExemption',
      params: [0, {commitment: 'singleGossip'}],
    },
    {
      error: null,
      result: 50,
    },
  ]);

  const minimumAmount = await connection.getMinimumBalanceForRentExemption(
    0,
    'singleGossip',
  );

  mockRpc.push([
    url,
    {
      method: 'requestAirdrop',
      params: [accountPayer.publicKey.toBase58(), LAMPORTS_PER_SAFE],
    },
    {
      error: null,
      result:
        '8WE5w4B7v59x6qjyC4FbG2FEKYKQfvsJwqSxNVmtMjT8TQ31hsZieDHcSgqzxiAoTL56n2w5TncjqEKjLhtF4Vk',
    },
  ]);
  let signature = await connection.requestAirdrop(
    accountPayer.publicKey,
    LAMPORTS_PER_SAFE,
  );
  mockConfirmTransaction(signature);
  await connection.confirmTransaction(signature, 'singleGossip');

  mockRpc.push([
    url,
    {
      method: 'requestAirdrop',
      params: [accountFrom.publicKey.toBase58(), minimumAmount + 12],
    },
    {
      error: null,
      result:
        '8WE5w4B7v59x6qjyC4FbG2FEKYKQfvsJwqSxNVmtMjT8TQ31hsZieDHcSgqzxiAoTL56n2w5TncjqEKjLhtF4Vk',
    },
  ]);
  signature = await connection.requestAirdrop(
    accountFrom.publicKey,
    minimumAmount + 12,
  );
  mockConfirmTransaction(signature);
  await connection.confirmTransaction(signature, 'singleGossip');

  mockRpc.push([
    url,
    {
      method: 'requestAirdrop',
      params: [accountTo.publicKey.toBase58(), minimumAmount + 21],
    },
    {
      error: null,
      result:
        '8WE5w4B7v59x6qjyC4FbG2FEKYKQfvsJwqSxNVmtMjT8TQ31hsZieDHcSgqzxiAoTL56n2w5TncjqEKjLhtF4Vk',
    },
  ]);
  signature = await connection.requestAirdrop(
    accountTo.publicKey,
    minimumAmount + 21,
  );
  mockConfirmTransaction(signature);
  await connection.confirmTransaction(signature, 'singleGossip');

  mockGetRecentBlockhash('max');
  mockRpc.push([
    url,
    {
      method: 'sendTransaction',
    },
    {
      error: null,
      result:
        '3WE5w4B7v59x6qjyC4FbG2FEKYKQfvsJwqSxNVmtMjT8TQ31hsZieDHcSgqzxiAoTL56n2w5TncjqEKjLhtF4Vk',
    },
  ]);

  const transaction = new Transaction().add(
    SystemProgram.transfer({
      fromPubkey: accountFrom.publicKey,
      toPubkey: accountTo.publicKey,
      lamports: 10,
    }),
  );

  signature = await connection.sendTransaction(
    transaction,
    [accountPayer, accountFrom],
    {skipPreflight: true},
  );

  mockConfirmTransaction(signature);
  await connection.confirmTransaction(signature, 'singleGossip');

  mockRpc.push([
    url,
    {
      method: 'getSignatureStatuses',
      params: [
        [
          '3WE5w4B7v59x6qjyC4FbG2FEKYKQfvsJwqSxNVmtMjT8TQ31hsZieDHcSgqzxiAoTL56n2w5TncjqEKjLhtF4Vk',
        ],
      ],
    },
    {
      error: null,
      result: {
        context: {
          slot: 11,
        },
        value: [
          {
            slot: 0,
            confirmations: 11,
            status: {Ok: null},
            err: null,
          },
        ],
      },
    },
  ]);
  const {value} = await connection.getSignatureStatus(signature);
  if (value !== null) {
    expect(typeof value.slot).toEqual('number');
    expect(value.err).toBeNull();
  } else {
    expect(value).not.toBeNull();
  }

  mockRpc.push([
    url,
    {
      method: 'getBalance',
      params: [accountPayer.publicKey.toBase58(), {commitment: 'singleGossip'}],
    },
    {
      error: null,
      result: {
        context: {
          slot: 11,
        },
        value: LAMPORTS_PER_SAFE - 1,
      },
    },
  ]);

  // accountPayer should be less than LAMPORTS_PER_SAFE as it paid for the transaction
  // (exact amount less depends on the current cluster fees)
  const balance = await connection.getBalance(accountPayer.publicKey);
  expect(balance).toBeGreaterThan(0);
  expect(balance).toBeLessThanOrEqual(LAMPORTS_PER_SAFE);

  // accountFrom should have exactly 2, since it didn't pay for the transaction
  mockRpc.push([
    url,
    {
      method: 'getBalance',
      params: [accountFrom.publicKey.toBase58(), {commitment: 'singleGossip'}],
    },
    {
      error: null,
      result: {
        context: {
          slot: 11,
        },
        value: minimumAmount + 2,
      },
    },
  ]);
  expect(await connection.getBalance(accountFrom.publicKey)).toBe(
    minimumAmount + 2,
  );
});
