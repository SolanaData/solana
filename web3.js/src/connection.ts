import bs58 from 'bs58';
import {Buffer} from 'buffer';
// @ts-ignore
import fastStableStringify from 'fast-stable-stringify';
import {
  type as pick,
  number,
  string,
  array,
  boolean,
  literal,
  record,
  union,
  optional,
  nullable,
  coerce,
  instance,
  create,
  tuple,
  unknown,
  any,
} from 'superstruct';
import type {Struct} from 'superstruct';
import {Client as RpcWebSocketClient} from 'rpc-websockets';
import RpcClient from 'jayson/lib/client/browser';

import {AgentManager} from './agent-manager';
import {EpochSchedule} from './epoch-schedule';
import {SendTransactionError} from './errors';
import fetchImpl, {Response} from './fetch-impl';
import {NonceAccount} from './nonce-account';
import {PublicKey} from './publickey';
import {Signer} from './keypair';
import {MS_PER_SLOT} from './timing';
import {Transaction, TransactionStatus} from './transaction';
import {Message} from './message';
import assert from './util/assert';
import {sleep} from './util/sleep';
import {toBuffer} from './util/to-buffer';
import {
  TransactionExpiredBlockheightExceededError,
  TransactionExpiredTimeoutError,
} from './util/tx-expiry-custom-errors';
import {makeWebsocketUrl} from './util/url';
import type {Blockhash} from './blockhash';
import type {FeeCalculator} from './fee-calculator';
import type {TransactionSignature} from './transaction';
import type {CompiledInstruction} from './message';

const PublicKeyFromString = coerce(
  instance(PublicKey),
  string(),
  value => new PublicKey(value),
);

const RawAccountDataResult = tuple([string(), literal('base64')]);

const BufferFromRawAccountData = coerce(
  instance(Buffer),
  RawAccountDataResult,
  value => Buffer.from(value[0], 'base64'),
);

/**
 * Attempt to use a recent blockhash for up to 30 seconds
 * @internal
 */
export const BLOCKHASH_CACHE_TIMEOUT_MS = 30 * 1000;

/**
 * HACK.
 * Copied from rpc-websockets/dist/lib/client.
 * Otherwise, `yarn build` fails with:
 * https://gist.github.com/steveluscher/c057eca81d479ef705cdb53162f9971d
 */
interface IWSRequestParams {
  [x: string]: any;
  [x: number]: any;
}

type ClientSubscriptionId = number;
/** @internal */ type ServerSubscriptionId = number;
/** @internal */ type SubscriptionConfigHash = string;
/** @internal */ type SubscriptionDisposeFn = () => Promise<void>;
/**
 * @internal
 * Every subscription contains the args used to open the subscription with
 * the server, and a list of callers interested in notifications.
 */
type BaseSubscription<TMethod = SubscriptionConfig['method']> = Readonly<{
  args: IWSRequestParams;
  callbacks: Set<Extract<SubscriptionConfig, {method: TMethod}>['callback']>;
}>;
/**
 * @internal
 * A subscription may be in various states of connectedness. Only when it is
 * fully connected will it have a server subscription id associated with it.
 * This id can be returned to the server to unsubscribe the client entirely.
 */
type StatefulSubscription = Readonly<
  // New subscriptions that have not yet been
  // sent to the server start in this state.
  | {
      state: 'pending';
    }
  // These subscriptions have been sent to the server
  // and are waiting for the server to acknowledge them.
  | {
      state: 'subscribing';
    }
  // These subscriptions have been acknowledged by the
  // server and have been assigned server subscription ids.
  | {
      serverSubscriptionId: ServerSubscriptionId;
      state: 'subscribed';
    }
  // These subscriptions are intended to be torn down and
  // are waiting on an acknowledgement from the server.
  | {
      serverSubscriptionId: ServerSubscriptionId;
      state: 'unsubscribing';
    }
  // The request to tear down these subscriptions has been
  // acknowledged by the server. The `serverSubscriptionId`
  // is the id of the now-dead subscription.
  | {
      serverSubscriptionId: ServerSubscriptionId;
      state: 'unsubscribed';
    }
>;
/**
 * A type that encapsulates a subscription's RPC method
 * names and notification (callback) signature.
 */
type SubscriptionConfig = Readonly<
  | {
      callback: AccountChangeCallback;
      method: 'accountSubscribe';
      unsubscribeMethod: 'accountUnsubscribe';
    }
  | {
      callback: LogsCallback;
      method: 'logsSubscribe';
      unsubscribeMethod: 'logsUnsubscribe';
    }
  | {
      callback: ProgramAccountChangeCallback;
      method: 'programSubscribe';
      unsubscribeMethod: 'programUnsubscribe';
    }
  | {
      callback: RootChangeCallback;
      method: 'rootSubscribe';
      unsubscribeMethod: 'rootUnsubscribe';
    }
  | {
      callback: SignatureSubscriptionCallback;
      method: 'signatureSubscribe';
      unsubscribeMethod: 'signatureUnsubscribe';
    }
  | {
      callback: SlotChangeCallback;
      method: 'slotSubscribe';
      unsubscribeMethod: 'slotUnsubscribe';
    }
  | {
      callback: SlotUpdateCallback;
      method: 'slotsUpdatesSubscribe';
      unsubscribeMethod: 'slotsUpdatesUnsubscribe';
    }
>;
/**
 * @internal
 * Utility type that keeps tagged unions intact while omitting properties.
 */
type DistributiveOmit<T, K extends PropertyKey> = T extends unknown
  ? Omit<T, K>
  : never;
/**
 * @internal
 * This type represents a single subscribable 'topic.' It's made up of:
 *
 * - The args used to open the subscription with the server,
 * - The state of the subscription, in terms of its connectedness, and
 * - The set of callbacks to call when the server publishes notifications
 *
 * This record gets indexed by `SubscriptionConfigHash` and is used to
 * set up subscriptions, fan out notifications, and track subscription state.
 */
type Subscription = BaseSubscription &
  StatefulSubscription &
  DistributiveOmit<SubscriptionConfig, 'callback'>;

type RpcRequest = (methodName: string, args: Array<any>) => Promise<any>;

type RpcBatchRequest = (requests: RpcParams[]) => Promise<any[]>;

/**
 * @internal
 */
export type RpcParams = {
  methodName: string;
  args: Array<any>;
};

export type TokenAccountsFilter =
  | {
      mint: PublicKey;
    }
  | {
      programId: PublicKey;
    };

/**
 * Extra contextual information for RPC responses
 */
export type Context = {
  slot: number;
};

/**
 * Options for sending transactions
 */
export type SendOptions = {
  /** disable transaction verification step */
  skipPreflight?: boolean;
  /** preflight commitment level */
  preflightCommitment?: Commitment;
  /** Maximum number of times for the RPC node to retry sending the transaction to the leader. */
  maxRetries?: number;
};

/**
 * Options for confirming transactions
 */
export type ConfirmOptions = {
  /** disable transaction verification step */
  skipPreflight?: boolean;
  /** desired commitment level */
  commitment?: Commitment;
  /** preflight commitment level */
  preflightCommitment?: Commitment;
  /** Maximum number of times for the RPC node to retry sending the transaction to the leader. */
  maxRetries?: number;
};

/**
 * Options for getConfirmedSignaturesForAddress2
 */
export type ConfirmedSignaturesForAddress2Options = {
  /**
   * Start searching backwards from this transaction signature.
   * @remark If not provided the search starts from the highest max confirmed block.
   */
  before?: TransactionSignature;
  /** Search until this transaction signature is reached, if found before `limit`. */
  until?: TransactionSignature;
  /** Maximum transaction signatures to return (between 1 and 1,000, default: 1,000). */
  limit?: number;
};

/**
 * Options for getSignaturesForAddress
 */
export type SignaturesForAddressOptions = {
  /**
   * Start searching backwards from this transaction signature.
   * @remark If not provided the search starts from the highest max confirmed block.
   */
  before?: TransactionSignature;
  /** Search until this transaction signature is reached, if found before `limit`. */
  until?: TransactionSignature;
  /** Maximum transaction signatures to return (between 1 and 1,000, default: 1,000). */
  limit?: number;
};

/**
 * RPC Response with extra contextual information
 */
export type RpcResponseAndContext<T> = {
  /** response context */
  context: Context;
  /** response value */
  value: T;
};

export type BlockhashWithExpiryBlockHeight = Readonly<{
  blockhash: Blockhash;
  lastValidBlockHeight: number;
}>;

/**
 * A strategy for confirming transactions that uses the last valid
 * block height for a given blockhash to check for transaction expiration.
 */
export type BlockheightBasedTransactionConfirmationStrategy = {
  signature: TransactionSignature;
} & BlockhashWithExpiryBlockHeight;

/** @internal */
function extractCommitmentFromConfig<TConfig>(
  commitmentOrConfig?: Commitment | ({commitment?: Commitment} & TConfig),
) {
  let commitment: Commitment | undefined;
  let config: Omit<TConfig, 'commitment'> | undefined;
  if (typeof commitmentOrConfig === 'string') {
    commitment = commitmentOrConfig;
  } else if (commitmentOrConfig) {
    const {commitment: specifiedCommitment, ...specifiedConfig} =
      commitmentOrConfig;
    commitment = specifiedCommitment;
    config = specifiedConfig;
  }
  return {commitment, config};
}

/**
 * @internal
 */
function createRpcResult<T, U>(result: Struct<T, U>) {
  return union([
    pick({
      jsonrpc: literal('2.0'),
      id: string(),
      result,
    }),
    pick({
      jsonrpc: literal('2.0'),
      id: string(),
      error: pick({
        code: unknown(),
        message: string(),
        data: optional(any()),
      }),
    }),
  ]);
}

const UnknownRpcResult = createRpcResult(unknown());

/**
 * @internal
 */
function jsonRpcResult<T, U>(schema: Struct<T, U>) {
  return coerce(createRpcResult(schema), UnknownRpcResult, value => {
    if ('error' in value) {
      return value;
    } else {
      return {
        ...value,
        result: create(value.result, schema),
      };
    }
  });
}

/**
 * @internal
 */
function jsonRpcResultAndContext<T, U>(value: Struct<T, U>) {
  return jsonRpcResult(
    pick({
      context: pick({
        slot: number(),
      }),
      value,
    }),
  );
}

/**
 * @internal
 */
function notificationResultAndContext<T, U>(value: Struct<T, U>) {
  return pick({
    context: pick({
      slot: number(),
    }),
    value,
  });
}

/**
 * The level of commitment desired when querying state
 * <pre>
 *   'processed': Query the most recent block which has reached 1 confirmation by the connected node
 *   'confirmed': Query the most recent block which has reached 1 confirmation by the cluster
 *   'finalized': Query the most recent block which has been finalized by the cluster
 * </pre>
 */
export type Commitment =
  | 'processed'
  | 'confirmed'
  | 'finalized'
  | 'recent' // Deprecated as of v1.5.5
  | 'single' // Deprecated as of v1.5.5
  | 'singleGossip' // Deprecated as of v1.5.5
  | 'root' // Deprecated as of v1.5.5
  | 'max'; // Deprecated as of v1.5.5

/**
 * A subset of Commitment levels, which are at least optimistically confirmed
 * <pre>
 *   'confirmed': Query the most recent block which has reached 1 confirmation by the cluster
 *   'finalized': Query the most recent block which has been finalized by the cluster
 * </pre>
 */
export type Finality = 'confirmed' | 'finalized';

/**
 * Filter for largest accounts query
 * <pre>
 *   'circulating':    Return the largest accounts that are part of the circulating supply
 *   'nonCirculating': Return the largest accounts that are not part of the circulating supply
 * </pre>
 */
export type LargestAccountsFilter = 'circulating' | 'nonCirculating';

/**
 * Configuration object for changing `getAccountInfo` query behavior
 */
export type GetAccountInfoConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** The minimum slot that the request can be evaluated at */
  minContextSlot?: number;
};

/**
 * Configuration object for changing `getBalance` query behavior
 */
export type GetBalanceConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** The minimum slot that the request can be evaluated at */
  minContextSlot?: number;
};

/**
 * Configuration object for changing `getBlockHeight` query behavior
 */
export type GetBlockHeightConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** The minimum slot that the request can be evaluated at */
  minContextSlot?: number;
};

/**
 * Configuration object for changing `getEpochInfo` query behavior
 */
export type GetEpochInfoConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** The minimum slot that the request can be evaluated at */
  minContextSlot?: number;
};

/**
 * Configuration object for changing `getInflationReward` query behavior
 */
export type GetInflationRewardConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** An epoch for which the reward occurs. If omitted, the previous epoch will be used */
  epoch?: number;
  /** The minimum slot that the request can be evaluated at */
  minContextSlot?: number;
};

/**
 * Configuration object for changing `getLatestBlockhash` query behavior
 */
export type GetLatestBlockhashConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** The minimum slot that the request can be evaluated at */
  minContextSlot?: number;
};

/**
 * Configuration object for changing `getLargestAccounts` query behavior
 */
export type GetLargestAccountsConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** Filter largest accounts by whether they are part of the circulating supply */
  filter?: LargestAccountsFilter;
};

/**
 * Configuration object for changing `getSupply` request behavior
 */
export type GetSupplyConfig = {
  /** The level of commitment desired */
  commitment?: Commitment;
  /** Exclude non circulating accounts list from response */
  excludeNonCirculatingAccountsList?: boolean;
};

/**
 * Configuration object for changing query behavior
 */
export type SignatureStatusConfig = {
  /** enable searching status history, not needed for recent transactions */
  searchTransactionHistory: boolean;
};

/**
 * Information describing a cluster node
 */
export type ContactInfo = {
  /** Identity public key of the node */
  pubkey: string;
  /** Gossip network address for the node */
  gossip: string | null;
  /** TPU network address for the node (null if not available) */
  tpu: string | null;
  /** JSON RPC network address for the node (null if not available) */
  rpc: string | null;
  /** Software version of the node (null if not available) */
  version: string | null;
};

/**
 * Information describing a vote account
 */
export type VoteAccountInfo = {
  /** Public key of the vote account */
  votePubkey: string;
  /** Identity public key of the node voting with this account */
  nodePubkey: string;
  /** The stake, in lamports, delegated to this vote account and activated */
  activatedStake: number;
  /** Whether the vote account is staked for this epoch */
  epochVoteAccount: boolean;
  /** Recent epoch voting credit history for this voter */
  epochCredits: Array<[number, number, number]>;
  /** A percentage (0-100) of rewards payout owed to the voter */
  commission: number;
  /** Most recent slot voted on by this vote account */
  lastVote: number;
};

/**
 * A collection of cluster vote accounts
 */
export type VoteAccountStatus = {
  /** Active vote accounts */
  current: Array<VoteAccountInfo>;
  /** Inactive vote accounts */
  delinquent: Array<VoteAccountInfo>;
};

/**
 * Network Inflation
 * (see https://docs.solana.com/implemented-proposals/ed_overview)
 */
export type InflationGovernor = {
  foundation: number;
  foundationTerm: number;
  initial: number;
  taper: number;
  terminal: number;
};

const GetInflationGovernorResult = pick({
  foundation: number(),
  foundationTerm: number(),
  initial: number(),
  taper: number(),
  terminal: number(),
});

/**
 * The inflation reward for an epoch
 */
export type InflationReward = {
  /** epoch for which the reward occurs */
  epoch: number;
  /** the slot in which the rewards are effective */
  effectiveSlot: number;
  /** reward amount in lamports */
  amount: number;
  /** post balance of the account in lamports */
  postBalance: number;
};

/**
 * Expected JSON RPC response for the "getInflationReward" message
 */
const GetInflationRewardResult = jsonRpcResult(
  array(
    nullable(
      pick({
        epoch: number(),
        effectiveSlot: number(),
        amount: number(),
        postBalance: number(),
      }),
    ),
  ),
);

/**
 * Information about the current epoch
 */
export type EpochInfo = {
  epoch: number;
  slotIndex: number;
  slotsInEpoch: number;
  absoluteSlot: number;
  blockHeight?: number;
  transactionCount?: number;
};

const GetEpochInfoResult = pick({
  epoch: number(),
  slotIndex: number(),
  slotsInEpoch: number(),
  absoluteSlot: number(),
  blockHeight: optional(number()),
  transactionCount: optional(number()),
});

const GetEpochScheduleResult = pick({
  slotsPerEpoch: number(),
  leaderScheduleSlotOffset: number(),
  warmup: boolean(),
  firstNormalEpoch: number(),
  firstNormalSlot: number(),
});

/**
 * Leader schedule
 * (see https://docs.solana.com/terminology#leader-schedule)
 */
export type LeaderSchedule = {
  [address: string]: number[];
};

const GetLeaderScheduleResult = record(string(), array(number()));

/**
 * Transaction error or null
 */
const TransactionErrorResult = nullable(union([pick({}), string()]));

/**
 * Signature status for a transaction
 */
const SignatureStatusResult = pick({
  err: TransactionErrorResult,
});

/**
 * Transaction signature received notification
 */
const SignatureReceivedResult = literal('receivedSignature');

/**
 * Version info for a node
 */
export type Version = {
  /** Version of solana-core */
  'solana-core': string;
  'feature-set'?: number;
};

const VersionResult = pick({
  'solana-core': string(),
  'feature-set': optional(number()),
});

export type SimulatedTransactionAccountInfo = {
  /** `true` if this account's data contains a loaded program */
  executable: boolean;
  /** Identifier of the program that owns the account */
  owner: string;
  /** Number of lamports assigned to the account */
  lamports: number;
  /** Optional data assigned to the account */
  data: string[];
  /** Optional rent epoch info for account */
  rentEpoch?: number;
};

export type SimulatedTransactionResponse = {
  err: TransactionError | string | null;
  logs: Array<string> | null;
  accounts?: (SimulatedTransactionAccountInfo | null)[] | null;
  unitsConsumed?: number;
};

const SimulatedTransactionResponseStruct = jsonRpcResultAndContext(
  pick({
    err: nullable(union([pick({}), string()])),
    logs: nullable(array(string())),
    accounts: optional(
      nullable(
        array(
          nullable(
            pick({
              executable: boolean(),
              owner: string(),
              lamports: number(),
              data: array(string()),
              rentEpoch: optional(number()),
            }),
          ),
        ),
      ),
    ),
    unitsConsumed: optional(number()),
  }),
);

export type ParsedInnerInstruction = {
  index: number;
  instructions: (ParsedInstruction | PartiallyDecodedInstruction)[];
};

export type TokenBalance = {
  accountIndex: number;
  mint: string;
  owner?: string;
  uiTokenAmount: TokenAmount;
};

/**
 * Metadata for a parsed confirmed transaction on the ledger
 *
 * @deprecated Deprecated since Solana v1.8.0. Please use {@link ParsedTransactionMeta} instead.
 */
export type ParsedConfirmedTransactionMeta = ParsedTransactionMeta;

/**
 * Metadata for a parsed transaction on the ledger
 */
export type ParsedTransactionMeta = {
  /** The fee charged for processing the transaction */
  fee: number;
  /** An array of cross program invoked parsed instructions */
  innerInstructions?: ParsedInnerInstruction[] | null;
  /** The balances of the transaction accounts before processing */
  preBalances: Array<number>;
  /** The balances of the transaction accounts after processing */
  postBalances: Array<number>;
  /** An array of program log messages emitted during a transaction */
  logMessages?: Array<string> | null;
  /** The token balances of the transaction accounts before processing */
  preTokenBalances?: Array<TokenBalance> | null;
  /** The token balances of the transaction accounts after processing */
  postTokenBalances?: Array<TokenBalance> | null;
  /** The error result of transaction processing */
  err: TransactionError | null;
};

export type CompiledInnerInstruction = {
  index: number;
  instructions: CompiledInstruction[];
};

/**
 * Metadata for a confirmed transaction on the ledger
 */
export type ConfirmedTransactionMeta = {
  /** The fee charged for processing the transaction */
  fee: number;
  /** An array of cross program invoked instructions */
  innerInstructions?: CompiledInnerInstruction[] | null;
  /** The balances of the transaction accounts before processing */
  preBalances: Array<number>;
  /** The balances of the transaction accounts after processing */
  postBalances: Array<number>;
  /** An array of program log messages emitted during a transaction */
  logMessages?: Array<string> | null;
  /** The token balances of the transaction accounts before processing */
  preTokenBalances?: Array<TokenBalance> | null;
  /** The token balances of the transaction accounts after processing */
  postTokenBalances?: Array<TokenBalance> | null;
  /** The error result of transaction processing */
  err: TransactionError | null;
};

/**
 * A processed transaction from the RPC API
 */
export type TransactionResponse = {
  /** The slot during which the transaction was processed */
  slot: number;
  /** The transaction */
  transaction: {
    /** The transaction message */
    message: Message;
    /** The transaction signatures */
    signatures: string[];
  };
  /** Metadata produced from the transaction */
  meta: ConfirmedTransactionMeta | null;
  /** The unix timestamp of when the transaction was processed */
  blockTime?: number | null;
};

/**
 * A confirmed transaction on the ledger
 */
export type ConfirmedTransaction = {
  /** The slot during which the transaction was processed */
  slot: number;
  /** The details of the transaction */
  transaction: Transaction;
  /** Metadata produced from the transaction */
  meta: ConfirmedTransactionMeta | null;
  /** The unix timestamp of when the transaction was processed */
  blockTime?: number | null;
};

/**
 * A partially decoded transaction instruction
 */
export type PartiallyDecodedInstruction = {
  /** Program id called by this instruction */
  programId: PublicKey;
  /** Public keys of accounts passed to this instruction */
  accounts: Array<PublicKey>;
  /** Raw base-58 instruction data */
  data: string;
};

/**
 * A parsed transaction message account
 */
export type ParsedMessageAccount = {
  /** Public key of the account */
  pubkey: PublicKey;
  /** Indicates if the account signed the transaction */
  signer: boolean;
  /** Indicates if the account is writable for this transaction */
  writable: boolean;
};

/**
 * A parsed transaction instruction
 */
export type ParsedInstruction = {
  /** Name of the program for this instruction */
  program: string;
  /** ID of the program for this instruction */
  programId: PublicKey;
  /** Parsed instruction info */
  parsed: any;
};

/**
 * A parsed transaction message
 */
export type ParsedMessage = {
  /** Accounts used in the instructions */
  accountKeys: ParsedMessageAccount[];
  /** The atomically executed instructions for the transaction */
  instructions: (ParsedInstruction | PartiallyDecodedInstruction)[];
  /** Recent blockhash */
  recentBlockhash: string;
};

/**
 * A parsed transaction
 */
export type ParsedTransaction = {
  /** Signatures for the transaction */
  signatures: Array<string>;
  /** Message of the transaction */
  message: ParsedMessage;
};

/**
 * A parsed and confirmed transaction on the ledger
 *
 * @deprecated Deprecated since Solana v1.8.0. Please use {@link ParsedTransactionWithMeta} instead.
 */
export type ParsedConfirmedTransaction = ParsedTransactionWithMeta;

/**
 * A parsed transaction on the ledger with meta
 */
export type ParsedTransactionWithMeta = {
  /** The slot during which the transaction was processed */
  slot: number;
  /** The details of the transaction */
  transaction: ParsedTransaction;
  /** Metadata produced from the transaction */
  meta: ParsedTransactionMeta | null;
  /** The unix timestamp of when the transaction was processed */
  blockTime?: number | null;
};

/**
 * A processed block fetched from the RPC API
 */
export type BlockResponse = {
  /** Blockhash of this block */
  blockhash: Blockhash;
  /** Blockhash of this block's parent */
  previousBlockhash: Blockhash;
  /** Slot index of this block's parent */
  parentSlot: number;
  /** Vector of transactions with status meta and original message */
  transactions: Array<{
    /** The transaction */
    transaction: {
      /** The transaction message */
      message: Message;
      /** The transaction signatures */
      signatures: string[];
    };
    /** Metadata produced from the transaction */
    meta: ConfirmedTransactionMeta | null;
  }>;
  /** Vector of block rewards */
  rewards?: Array<{
    /** Public key of reward recipient */
    pubkey: string;
    /** Reward value in lamports */
    lamports: number;
    /** Account balance after reward is applied */
    postBalance: number | null;
    /** Type of reward received */
    rewardType: string | null;
  }>;
  /** The unix timestamp of when the block was processed */
  blockTime: number | null;
};

/**
 * A ConfirmedBlock on the ledger
 */
export type ConfirmedBlock = {
  /** Blockhash of this block */
  blockhash: Blockhash;
  /** Blockhash of this block's parent */
  previousBlockhash: Blockhash;
  /** Slot index of this block's parent */
  parentSlot: number;
  /** Vector of transactions and status metas */
  transactions: Array<{
    transaction: Transaction;
    meta: ConfirmedTransactionMeta | null;
  }>;
  /** Vector of block rewards */
  rewards?: Array<{
    pubkey: string;
    lamports: number;
    postBalance: number | null;
    rewardType: string | null;
  }>;
  /** The unix timestamp of when the block was processed */
  blockTime: number | null;
};

/**
 * A Block on the ledger with signatures only
 */
export type BlockSignatures = {
  /** Blockhash of this block */
  blockhash: Blockhash;
  /** Blockhash of this block's parent */
  previousBlockhash: Blockhash;
  /** Slot index of this block's parent */
  parentSlot: number;
  /** Vector of signatures */
  signatures: Array<string>;
  /** The unix timestamp of when the block was processed */
  blockTime: number | null;
};

/**
 * recent block production information
 */
export type BlockProduction = Readonly<{
  /** a dictionary of validator identities, as base-58 encoded strings. Value is a two element array containing the number of leader slots and the number of blocks produced */
  byIdentity: Readonly<Record<string, ReadonlyArray<number>>>;
  /** Block production slot range */
  range: Readonly<{
    /** first slot of the block production information (inclusive) */
    firstSlot: number;
    /** last slot of block production information (inclusive) */
    lastSlot: number;
  }>;
}>;

export type GetBlockProductionConfig = {
  /** Optional commitment level */
  commitment?: Commitment;
  /** Slot range to return block production for. If parameter not provided, defaults to current epoch. */
  range?: {
    /** first slot to return block production information for (inclusive) */
    firstSlot: number;
    /** last slot to return block production information for (inclusive). If parameter not provided, defaults to the highest slot */
    lastSlot?: number;
  };
  /** Only return results for this validator identity (base-58 encoded) */
  identity?: string;
};

/**
 * Expected JSON RPC response for the "getBlockProduction" message
 */
const BlockProductionResponseStruct = jsonRpcResultAndContext(
  pick({
    byIdentity: record(string(), array(number())),
    range: pick({
      firstSlot: number(),
      lastSlot: number(),
    }),
  }),
);

/**
 * A performance sample
 */
export type PerfSample = {
  /** Slot number of sample */
  slot: number;
  /** Number of transactions in a sample window */
  numTransactions: number;
  /** Number of slots in a sample window */
  numSlots: number;
  /** Sample window in seconds */
  samplePeriodSecs: number;
};

function createRpcClient(
  url: string,
  useHttps: boolean,
  httpHeaders?: HttpHeaders,
  customFetch?: FetchFn,
  fetchMiddleware?: FetchMiddleware,
  disableRetryOnRateLimit?: boolean,
): RpcClient {
  const fetch = customFetch ? customFetch : fetchImpl;
  let agentManager: AgentManager | undefined;
  if (!process.env.BROWSER) {
    agentManager = new AgentManager(useHttps);
  }

  let fetchWithMiddleware: FetchFn | undefined;

  if (fetchMiddleware) {
    fetchWithMiddleware = async (info, init) => {
      const modifiedFetchArgs = await new Promise<Parameters<FetchFn>>(
        (resolve, reject) => {
          try {
            fetchMiddleware(info, init, (modifiedInfo, modifiedInit) =>
              resolve([modifiedInfo, modifiedInit]),
            );
          } catch (error) {
            reject(error);
          }
        },
      );
      return await fetch(...modifiedFetchArgs);
    };
  }

  const clientBrowser = new RpcClient(async (request, callback) => {
    const agent = agentManager ? agentManager.requestStart() : undefined;
    const options = {
      method: 'POST',
      body: request,
      agent,
      headers: Object.assign(
        {
          'Content-Type': 'application/json',
        },
        httpHeaders || {},
      ),
    };

    try {
      let too_many_requests_retries = 5;
      let res: Response;
      let waitTime = 500;
      for (;;) {
        if (fetchWithMiddleware) {
          res = await fetchWithMiddleware(url, options);
        } else {
          res = await fetch(url, options);
        }

        if (res.status !== 429 /* Too many requests */) {
          break;
        }
        if (disableRetryOnRateLimit === true) {
          break;
        }
        too_many_requests_retries -= 1;
        if (too_many_requests_retries === 0) {
          break;
        }
        console.log(
          `Server responded with ${res.status} ${res.statusText}.  Retrying after ${waitTime}ms delay...`,
        );
        await sleep(waitTime);
        waitTime *= 2;
      }

      const text = await res.text();
      if (res.ok) {
        callback(null, text);
      } else {
        callback(new Error(`${res.status} ${res.statusText}: ${text}`));
      }
    } catch (err) {
      if (err instanceof Error) callback(err);
    } finally {
      agentManager && agentManager.requestEnd();
    }
  }, {});

  return clientBrowser;
}

function createRpcRequest(client: RpcClient): RpcRequest {
  return (method, args) => {
    return new Promise((resolve, reject) => {
      client.request(method, args, (err: any, response: any) => {
        if (err) {
          reject(err);
          return;
        }
        resolve(response);
      });
    });
  };
}

function createRpcBatchRequest(client: RpcClient): RpcBatchRequest {
  return (requests: RpcParams[]) => {
    return new Promise((resolve, reject) => {
      // Do nothing if requests is empty
      if (requests.length === 0) resolve([]);

      const batch = requests.map((params: RpcParams) => {
        return client.request(params.methodName, params.args);
      });

      client.request(batch, (err: any, response: any) => {
        if (err) {
          reject(err);
          return;
        }
        resolve(response);
      });
    });
  };
}

/**
 * Expected JSON RPC response for the "getInflationGovernor" message
 */
const GetInflationGovernorRpcResult = jsonRpcResult(GetInflationGovernorResult);

/**
 * Expected JSON RPC response for the "getEpochInfo" message
 */
const GetEpochInfoRpcResult = jsonRpcResult(GetEpochInfoResult);

/**
 * Expected JSON RPC response for the "getEpochSchedule" message
 */
const GetEpochScheduleRpcResult = jsonRpcResult(GetEpochScheduleResult);

/**
 * Expected JSON RPC response for the "getLeaderSchedule" message
 */
const GetLeaderScheduleRpcResult = jsonRpcResult(GetLeaderScheduleResult);

/**
 * Expected JSON RPC response for the "minimumLedgerSlot" and "getFirstAvailableBlock" messages
 */
const SlotRpcResult = jsonRpcResult(number());

/**
 * Supply
 */
export type Supply = {
  /** Total supply in lamports */
  total: number;
  /** Circulating supply in lamports */
  circulating: number;
  /** Non-circulating supply in lamports */
  nonCirculating: number;
  /** List of non-circulating account addresses */
  nonCirculatingAccounts: Array<PublicKey>;
};

/**
 * Expected JSON RPC response for the "getSupply" message
 */
const GetSupplyRpcResult = jsonRpcResultAndContext(
  pick({
    total: number(),
    circulating: number(),
    nonCirculating: number(),
    nonCirculatingAccounts: array(PublicKeyFromString),
  }),
);

/**
 * Token amount object which returns a token amount in different formats
 * for various client use cases.
 */
export type TokenAmount = {
  /** Raw amount of tokens as string ignoring decimals */
  amount: string;
  /** Number of decimals configured for token's mint */
  decimals: number;
  /** Token amount as float, accounts for decimals */
  uiAmount: number | null;
  /** Token amount as string, accounts for decimals */
  uiAmountString?: string;
};

/**
 * Expected JSON RPC structure for token amounts
 */
const TokenAmountResult = pick({
  amount: string(),
  uiAmount: nullable(number()),
  decimals: number(),
  uiAmountString: optional(string()),
});

/**
 * Token address and balance.
 */
export type TokenAccountBalancePair = {
  /** Address of the token account */
  address: PublicKey;
  /** Raw amount of tokens as string ignoring decimals */
  amount: string;
  /** Number of decimals configured for token's mint */
  decimals: number;
  /** Token amount as float, accounts for decimals */
  uiAmount: number | null;
  /** Token amount as string, accounts for decimals */
  uiAmountString?: string;
};

/**
 * Expected JSON RPC response for the "getTokenLargestAccounts" message
 */
const GetTokenLargestAccountsResult = jsonRpcResultAndContext(
  array(
    pick({
      address: PublicKeyFromString,
      amount: string(),
      uiAmount: nullable(number()),
      decimals: number(),
      uiAmountString: optional(string()),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "getTokenAccountsByOwner" message
 */
const GetTokenAccountsByOwner = jsonRpcResultAndContext(
  array(
    pick({
      pubkey: PublicKeyFromString,
      account: pick({
        executable: boolean(),
        owner: PublicKeyFromString,
        lamports: number(),
        data: BufferFromRawAccountData,
        rentEpoch: number(),
      }),
    }),
  ),
);

const ParsedAccountDataResult = pick({
  program: string(),
  parsed: unknown(),
  space: number(),
});

/**
 * Expected JSON RPC response for the "getTokenAccountsByOwner" message with parsed data
 */
const GetParsedTokenAccountsByOwner = jsonRpcResultAndContext(
  array(
    pick({
      pubkey: PublicKeyFromString,
      account: pick({
        executable: boolean(),
        owner: PublicKeyFromString,
        lamports: number(),
        data: ParsedAccountDataResult,
        rentEpoch: number(),
      }),
    }),
  ),
);

/**
 * Pair of an account address and its balance
 */
export type AccountBalancePair = {
  address: PublicKey;
  lamports: number;
};

/**
 * Expected JSON RPC response for the "getLargestAccounts" message
 */
const GetLargestAccountsRpcResult = jsonRpcResultAndContext(
  array(
    pick({
      lamports: number(),
      address: PublicKeyFromString,
    }),
  ),
);

/**
 * @internal
 */
const AccountInfoResult = pick({
  executable: boolean(),
  owner: PublicKeyFromString,
  lamports: number(),
  data: BufferFromRawAccountData,
  rentEpoch: number(),
});

/**
 * @internal
 */
const KeyedAccountInfoResult = pick({
  pubkey: PublicKeyFromString,
  account: AccountInfoResult,
});

const ParsedOrRawAccountData = coerce(
  union([instance(Buffer), ParsedAccountDataResult]),
  union([RawAccountDataResult, ParsedAccountDataResult]),
  value => {
    if (Array.isArray(value)) {
      return create(value, BufferFromRawAccountData);
    } else {
      return value;
    }
  },
);

/**
 * @internal
 */
const ParsedAccountInfoResult = pick({
  executable: boolean(),
  owner: PublicKeyFromString,
  lamports: number(),
  data: ParsedOrRawAccountData,
  rentEpoch: number(),
});

const KeyedParsedAccountInfoResult = pick({
  pubkey: PublicKeyFromString,
  account: ParsedAccountInfoResult,
});

/**
 * @internal
 */
const StakeActivationResult = pick({
  state: union([
    literal('active'),
    literal('inactive'),
    literal('activating'),
    literal('deactivating'),
  ]),
  active: number(),
  inactive: number(),
});

/**
 * Expected JSON RPC response for the "getConfirmedSignaturesForAddress2" message
 */

const GetConfirmedSignaturesForAddress2RpcResult = jsonRpcResult(
  array(
    pick({
      signature: string(),
      slot: number(),
      err: TransactionErrorResult,
      memo: nullable(string()),
      blockTime: optional(nullable(number())),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "getSignaturesForAddress" message
 */
const GetSignaturesForAddressRpcResult = jsonRpcResult(
  array(
    pick({
      signature: string(),
      slot: number(),
      err: TransactionErrorResult,
      memo: nullable(string()),
      blockTime: optional(nullable(number())),
    }),
  ),
);

/***
 * Expected JSON RPC response for the "accountNotification" message
 */
const AccountNotificationResult = pick({
  subscription: number(),
  result: notificationResultAndContext(AccountInfoResult),
});

/**
 * @internal
 */
const ProgramAccountInfoResult = pick({
  pubkey: PublicKeyFromString,
  account: AccountInfoResult,
});

/***
 * Expected JSON RPC response for the "programNotification" message
 */
const ProgramAccountNotificationResult = pick({
  subscription: number(),
  result: notificationResultAndContext(ProgramAccountInfoResult),
});

/**
 * @internal
 */
const SlotInfoResult = pick({
  parent: number(),
  slot: number(),
  root: number(),
});

/**
 * Expected JSON RPC response for the "slotNotification" message
 */
const SlotNotificationResult = pick({
  subscription: number(),
  result: SlotInfoResult,
});

/**
 * Slot updates which can be used for tracking the live progress of a cluster.
 * - `"firstShredReceived"`: connected node received the first shred of a block.
 * Indicates that a new block that is being produced.
 * - `"completed"`: connected node has received all shreds of a block. Indicates
 * a block was recently produced.
 * - `"optimisticConfirmation"`: block was optimistically confirmed by the
 * cluster. It is not guaranteed that an optimistic confirmation notification
 * will be sent for every finalized blocks.
 * - `"root"`: the connected node rooted this block.
 * - `"createdBank"`: the connected node has started validating this block.
 * - `"frozen"`: the connected node has validated this block.
 * - `"dead"`: the connected node failed to validate this block.
 */
export type SlotUpdate =
  | {
      type: 'firstShredReceived';
      slot: number;
      timestamp: number;
    }
  | {
      type: 'completed';
      slot: number;
      timestamp: number;
    }
  | {
      type: 'createdBank';
      slot: number;
      timestamp: number;
      parent: number;
    }
  | {
      type: 'frozen';
      slot: number;
      timestamp: number;
      stats: {
        numTransactionEntries: number;
        numSuccessfulTransactions: number;
        numFailedTransactions: number;
        maxTransactionsPerEntry: number;
      };
    }
  | {
      type: 'dead';
      slot: number;
      timestamp: number;
      err: string;
    }
  | {
      type: 'optimisticConfirmation';
      slot: number;
      timestamp: number;
    }
  | {
      type: 'root';
      slot: number;
      timestamp: number;
    };

/**
 * @internal
 */
const SlotUpdateResult = union([
  pick({
    type: union([
      literal('firstShredReceived'),
      literal('completed'),
      literal('optimisticConfirmation'),
      literal('root'),
    ]),
    slot: number(),
    timestamp: number(),
  }),
  pick({
    type: literal('createdBank'),
    parent: number(),
    slot: number(),
    timestamp: number(),
  }),
  pick({
    type: literal('frozen'),
    slot: number(),
    timestamp: number(),
    stats: pick({
      numTransactionEntries: number(),
      numSuccessfulTransactions: number(),
      numFailedTransactions: number(),
      maxTransactionsPerEntry: number(),
    }),
  }),
  pick({
    type: literal('dead'),
    slot: number(),
    timestamp: number(),
    err: string(),
  }),
]);

/**
 * Expected JSON RPC response for the "slotsUpdatesNotification" message
 */
const SlotUpdateNotificationResult = pick({
  subscription: number(),
  result: SlotUpdateResult,
});

/**
 * Expected JSON RPC response for the "signatureNotification" message
 */
const SignatureNotificationResult = pick({
  subscription: number(),
  result: notificationResultAndContext(
    union([SignatureStatusResult, SignatureReceivedResult]),
  ),
});

/**
 * Expected JSON RPC response for the "rootNotification" message
 */
const RootNotificationResult = pick({
  subscription: number(),
  result: number(),
});

const ContactInfoResult = pick({
  pubkey: string(),
  gossip: nullable(string()),
  tpu: nullable(string()),
  rpc: nullable(string()),
  version: nullable(string()),
});

const VoteAccountInfoResult = pick({
  votePubkey: string(),
  nodePubkey: string(),
  activatedStake: number(),
  epochVoteAccount: boolean(),
  epochCredits: array(tuple([number(), number(), number()])),
  commission: number(),
  lastVote: number(),
  rootSlot: nullable(number()),
});

/**
 * Expected JSON RPC response for the "getVoteAccounts" message
 */
const GetVoteAccounts = jsonRpcResult(
  pick({
    current: array(VoteAccountInfoResult),
    delinquent: array(VoteAccountInfoResult),
  }),
);

const ConfirmationStatus = union([
  literal('processed'),
  literal('confirmed'),
  literal('finalized'),
]);

const SignatureStatusResponse = pick({
  slot: number(),
  confirmations: nullable(number()),
  err: TransactionErrorResult,
  confirmationStatus: optional(ConfirmationStatus),
});

/**
 * Expected JSON RPC response for the "getSignatureStatuses" message
 */
const GetSignatureStatusesRpcResult = jsonRpcResultAndContext(
  array(nullable(SignatureStatusResponse)),
);

/**
 * Expected JSON RPC response for the "getMinimumBalanceForRentExemption" message
 */
const GetMinimumBalanceForRentExemptionRpcResult = jsonRpcResult(number());

const ConfirmedTransactionResult = pick({
  signatures: array(string()),
  message: pick({
    accountKeys: array(string()),
    header: pick({
      numRequiredSignatures: number(),
      numReadonlySignedAccounts: number(),
      numReadonlyUnsignedAccounts: number(),
    }),
    instructions: array(
      pick({
        accounts: array(number()),
        data: string(),
        programIdIndex: number(),
      }),
    ),
    recentBlockhash: string(),
  }),
});

const ParsedInstructionResult = pick({
  parsed: unknown(),
  program: string(),
  programId: PublicKeyFromString,
});

const RawInstructionResult = pick({
  accounts: array(PublicKeyFromString),
  data: string(),
  programId: PublicKeyFromString,
});

const InstructionResult = union([
  RawInstructionResult,
  ParsedInstructionResult,
]);

const UnknownInstructionResult = union([
  pick({
    parsed: unknown(),
    program: string(),
    programId: string(),
  }),
  pick({
    accounts: array(string()),
    data: string(),
    programId: string(),
  }),
]);

const ParsedOrRawInstruction = coerce(
  InstructionResult,
  UnknownInstructionResult,
  value => {
    if ('accounts' in value) {
      return create(value, RawInstructionResult);
    } else {
      return create(value, ParsedInstructionResult);
    }
  },
);

/**
 * @internal
 */
const ParsedConfirmedTransactionResult = pick({
  signatures: array(string()),
  message: pick({
    accountKeys: array(
      pick({
        pubkey: PublicKeyFromString,
        signer: boolean(),
        writable: boolean(),
      }),
    ),
    instructions: array(ParsedOrRawInstruction),
    recentBlockhash: string(),
  }),
});

const TokenBalanceResult = pick({
  accountIndex: number(),
  mint: string(),
  owner: optional(string()),
  uiTokenAmount: TokenAmountResult,
});

/**
 * @internal
 */
const ConfirmedTransactionMetaResult = pick({
  err: TransactionErrorResult,
  fee: number(),
  innerInstructions: optional(
    nullable(
      array(
        pick({
          index: number(),
          instructions: array(
            pick({
              accounts: array(number()),
              data: string(),
              programIdIndex: number(),
            }),
          ),
        }),
      ),
    ),
  ),
  preBalances: array(number()),
  postBalances: array(number()),
  logMessages: optional(nullable(array(string()))),
  preTokenBalances: optional(nullable(array(TokenBalanceResult))),
  postTokenBalances: optional(nullable(array(TokenBalanceResult))),
});

/**
 * @internal
 */
const ParsedConfirmedTransactionMetaResult = pick({
  err: TransactionErrorResult,
  fee: number(),
  innerInstructions: optional(
    nullable(
      array(
        pick({
          index: number(),
          instructions: array(ParsedOrRawInstruction),
        }),
      ),
    ),
  ),
  preBalances: array(number()),
  postBalances: array(number()),
  logMessages: optional(nullable(array(string()))),
  preTokenBalances: optional(nullable(array(TokenBalanceResult))),
  postTokenBalances: optional(nullable(array(TokenBalanceResult))),
});

/**
 * Expected JSON RPC response for the "getBlock" message
 */
const GetBlockRpcResult = jsonRpcResult(
  nullable(
    pick({
      blockhash: string(),
      previousBlockhash: string(),
      parentSlot: number(),
      transactions: array(
        pick({
          transaction: ConfirmedTransactionResult,
          meta: nullable(ConfirmedTransactionMetaResult),
        }),
      ),
      rewards: optional(
        array(
          pick({
            pubkey: string(),
            lamports: number(),
            postBalance: nullable(number()),
            rewardType: nullable(string()),
          }),
        ),
      ),
      blockTime: nullable(number()),
      blockHeight: nullable(number()),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "getConfirmedBlock" message
 *
 * @deprecated Deprecated since Solana v1.8.0. Please use {@link GetBlockRpcResult} instead.
 */
const GetConfirmedBlockRpcResult = jsonRpcResult(
  nullable(
    pick({
      blockhash: string(),
      previousBlockhash: string(),
      parentSlot: number(),
      transactions: array(
        pick({
          transaction: ConfirmedTransactionResult,
          meta: nullable(ConfirmedTransactionMetaResult),
        }),
      ),
      rewards: optional(
        array(
          pick({
            pubkey: string(),
            lamports: number(),
            postBalance: nullable(number()),
            rewardType: nullable(string()),
          }),
        ),
      ),
      blockTime: nullable(number()),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "getBlock" message
 */
const GetBlockSignaturesRpcResult = jsonRpcResult(
  nullable(
    pick({
      blockhash: string(),
      previousBlockhash: string(),
      parentSlot: number(),
      signatures: array(string()),
      blockTime: nullable(number()),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "getTransaction" message
 */
const GetTransactionRpcResult = jsonRpcResult(
  nullable(
    pick({
      slot: number(),
      meta: ConfirmedTransactionMetaResult,
      blockTime: optional(nullable(number())),
      transaction: ConfirmedTransactionResult,
    }),
  ),
);

/**
 * Expected parsed JSON RPC response for the "getTransaction" message
 */
const GetParsedTransactionRpcResult = jsonRpcResult(
  nullable(
    pick({
      slot: number(),
      transaction: ParsedConfirmedTransactionResult,
      meta: nullable(ParsedConfirmedTransactionMetaResult),
      blockTime: optional(nullable(number())),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "getRecentBlockhash" message
 *
 * @deprecated Deprecated since Solana v1.8.0. Please use {@link GetLatestBlockhashRpcResult} instead.
 */
const GetRecentBlockhashAndContextRpcResult = jsonRpcResultAndContext(
  pick({
    blockhash: string(),
    feeCalculator: pick({
      lamportsPerSignature: number(),
    }),
  }),
);

/**
 * Expected JSON RPC response for the "getLatestBlockhash" message
 */
const GetLatestBlockhashRpcResult = jsonRpcResultAndContext(
  pick({
    blockhash: string(),
    lastValidBlockHeight: number(),
  }),
);

const PerfSampleResult = pick({
  slot: number(),
  numTransactions: number(),
  numSlots: number(),
  samplePeriodSecs: number(),
});

/*
 * Expected JSON RPC response for "getRecentPerformanceSamples" message
 */
const GetRecentPerformanceSamplesRpcResult = jsonRpcResult(
  array(PerfSampleResult),
);

/**
 * Expected JSON RPC response for the "getFeeCalculatorForBlockhash" message
 */
const GetFeeCalculatorRpcResult = jsonRpcResultAndContext(
  nullable(
    pick({
      feeCalculator: pick({
        lamportsPerSignature: number(),
      }),
    }),
  ),
);

/**
 * Expected JSON RPC response for the "requestAirdrop" message
 */
const RequestAirdropRpcResult = jsonRpcResult(string());

/**
 * Expected JSON RPC response for the "sendTransaction" message
 */
const SendTransactionRpcResult = jsonRpcResult(string());

/**
 * Information about the latest slot being processed by a node
 */
export type SlotInfo = {
  /** Currently processing slot */
  slot: number;
  /** Parent of the current slot */
  parent: number;
  /** The root block of the current slot's fork */
  root: number;
};

/**
 * Parsed account data
 */
export type ParsedAccountData = {
  /** Name of the program that owns this account */
  program: string;
  /** Parsed account data */
  parsed: any;
  /** Space used by account data */
  space: number;
};

/**
 * Stake Activation data
 */
export type StakeActivationData = {
  /** the stake account's activation state */
  state: 'active' | 'inactive' | 'activating' | 'deactivating';
  /** stake active during the epoch */
  active: number;
  /** stake inactive during the epoch */
  inactive: number;
};

/**
 * Data slice argument for getProgramAccounts
 */
export type DataSlice = {
  /** offset of data slice */
  offset: number;
  /** length of data slice */
  length: number;
};

/**
 * Memory comparison filter for getProgramAccounts
 */
export type MemcmpFilter = {
  memcmp: {
    /** offset into program account data to start comparison */
    offset: number;
    /** data to match, as base-58 encoded string and limited to less than 129 bytes */
    bytes: string;
  };
};

/**
 * Data size comparison filter for getProgramAccounts
 */
export type DataSizeFilter = {
  /** Size of data for program account data length comparison */
  dataSize: number;
};

/**
 * A filter object for getProgramAccounts
 */
export type GetProgramAccountsFilter = MemcmpFilter | DataSizeFilter;

/**
 * Configuration object for getProgramAccounts requests
 */
export type GetProgramAccountsConfig = {
  /** Optional commitment level */
  commitment?: Commitment;
  /** Optional encoding for account data (default base64)
   * To use "jsonParsed" encoding, please refer to `getParsedProgramAccounts` in connection.ts
   * */
  encoding?: 'base64';
  /** Optional data slice to limit the returned account data */
  dataSlice?: DataSlice;
  /** Optional array of filters to apply to accounts */
  filters?: GetProgramAccountsFilter[];
};

/**
 * Configuration object for getParsedProgramAccounts
 */
export type GetParsedProgramAccountsConfig = {
  /** Optional commitment level */
  commitment?: Commitment;
  /** Optional array of filters to apply to accounts */
  filters?: GetProgramAccountsFilter[];
};

/**
 * Configuration object for getMultipleAccounts
 */
export type GetMultipleAccountsConfig = {
  /** Optional commitment level */
  commitment?: Commitment;
  /** Optional encoding for account data (default base64) */
  encoding?: 'base64' | 'jsonParsed';
};

/**
 * Information describing an account
 */
export type AccountInfo<T> = {
  /** `true` if this account's data contains a loaded program */
  executable: boolean;
  /** Identifier of the program that owns the account */
  owner: PublicKey;
  /** Number of lamports assigned to the account */
  lamports: number;
  /** Optional data assigned to the account */
  data: T;
  /** Optional rent epoch info for account */
  rentEpoch?: number;
};

/**
 * Account information identified by pubkey
 */
export type KeyedAccountInfo = {
  accountId: PublicKey;
  accountInfo: AccountInfo<Buffer>;
};

/**
 * Callback function for account change notifications
 */
export type AccountChangeCallback = (
  accountInfo: AccountInfo<Buffer>,
  context: Context,
) => void;

/**
 * Callback function for program account change notifications
 */
export type ProgramAccountChangeCallback = (
  keyedAccountInfo: KeyedAccountInfo,
  context: Context,
) => void;

/**
 * Callback function for slot change notifications
 */
export type SlotChangeCallback = (slotInfo: SlotInfo) => void;

/**
 * Callback function for slot update notifications
 */
export type SlotUpdateCallback = (slotUpdate: SlotUpdate) => void;

/**
 * Callback function for signature status notifications
 */
export type SignatureResultCallback = (
  signatureResult: SignatureResult,
  context: Context,
) => void;

/**
 * Signature status notification with transaction result
 */
export type SignatureStatusNotification = {
  type: 'status';
  result: SignatureResult;
};

/**
 * Signature received notification
 */
export type SignatureReceivedNotification = {
  type: 'received';
};

/**
 * Callback function for signature notifications
 */
export type SignatureSubscriptionCallback = (
  notification: SignatureStatusNotification | SignatureReceivedNotification,
  context: Context,
) => void;

/**
 * Signature subscription options
 */
export type SignatureSubscriptionOptions = {
  commitment?: Commitment;
  enableReceivedNotification?: boolean;
};

/**
 * Callback function for root change notifications
 */
export type RootChangeCallback = (root: number) => void;

/**
 * @internal
 */
const LogsResult = pick({
  err: TransactionErrorResult,
  logs: array(string()),
  signature: string(),
});

/**
 * Logs result.
 */
export type Logs = {
  err: TransactionError | null;
  logs: string[];
  signature: string;
};

/**
 * Expected JSON RPC response for the "logsNotification" message.
 */
const LogsNotificationResult = pick({
  result: notificationResultAndContext(LogsResult),
  subscription: number(),
});

/**
 * Filter for log subscriptions.
 */
export type LogsFilter = PublicKey | 'all' | 'allWithVotes';

/**
 * Callback function for log notifications.
 */
export type LogsCallback = (logs: Logs, ctx: Context) => void;

/**
 * Signature result
 */
export type SignatureResult = {
  err: TransactionError | null;
};

/**
 * Transaction error
 */
export type TransactionError = {} | string;

/**
 * Transaction confirmation status
 * <pre>
 *   'processed': Transaction landed in a block which has reached 1 confirmation by the connected node
 *   'confirmed': Transaction landed in a block which has reached 1 confirmation by the cluster
 *   'finalized': Transaction landed in a block which has been finalized by the cluster
 * </pre>
 */
export type TransactionConfirmationStatus =
  | 'processed'
  | 'confirmed'
  | 'finalized';

/**
 * Signature status
 */
export type SignatureStatus = {
  /** when the transaction was processed */
  slot: number;
  /** the number of blocks that have been confirmed and voted on in the fork containing `slot` */
  confirmations: number | null;
  /** transaction error, if any */
  err: TransactionError | null;
  /** cluster confirmation status, if data available. Possible responses: `processed`, `confirmed`, `finalized` */
  confirmationStatus?: TransactionConfirmationStatus;
};

/**
 * A confirmed signature with its status
 */
export type ConfirmedSignatureInfo = {
  /** the transaction signature */
  signature: string;
  /** when the transaction was processed */
  slot: number;
  /** error, if any */
  err: TransactionError | null;
  /** memo associated with the transaction, if any */
  memo: string | null;
  /** The unix timestamp of when the transaction was processed */
  blockTime?: number | null;
};

/**
 * An object defining headers to be passed to the RPC server
 */
export type HttpHeaders = {[header: string]: string};

/**
 * The type of the JavaScript `fetch()` API
 */
export type FetchFn = typeof fetchImpl;

/**
 * A callback used to augment the outgoing HTTP request
 */
export type FetchMiddleware = (
  info: Parameters<FetchFn>[0],
  init: Parameters<FetchFn>[1],
  fetch: (...a: Parameters<FetchFn>) => void,
) => void;

/**
 * Configuration for instantiating a Connection
 */
export type ConnectionConfig = {
  /** Optional commitment level */
  commitment?: Commitment;
  /** Optional endpoint URL to the fullnode JSON RPC PubSub WebSocket Endpoint */
  wsEndpoint?: string;
  /** Optional HTTP headers object */
  httpHeaders?: HttpHeaders;
  /** Optional custom fetch function */
  fetch?: FetchFn;
  /** Optional fetch middleware callback */
  fetchMiddleware?: FetchMiddleware;
  /** Optional Disable retrying calls when server responds with HTTP 429 (Too Many Requests) */
  disableRetryOnRateLimit?: boolean;
  /** time to allow for the server to initially process a transaction (in milliseconds) */
  confirmTransactionInitialTimeout?: number;
};

/**
 * A connection to a fullnode JSON RPC endpoint
 */
export class Connection {
  /** @internal */ _commitment?: Commitment;
  /** @internal */ _confirmTransactionInitialTimeout?: number;
  /** @internal */ _rpcEndpoint: string;
  /** @internal */ _rpcWsEndpoint: string;
  /** @internal */ _rpcClient: RpcClient;
  /** @internal */ _rpcRequest: RpcRequest;
  /** @internal */ _rpcBatchRequest: RpcBatchRequest;
  /** @internal */ _rpcWebSocket: RpcWebSocketClient;
  /** @internal */ _rpcWebSocketConnected: boolean = false;
  /** @internal */ _rpcWebSocketHeartbeat: ReturnType<
    typeof setInterval
  > | null = null;
  /** @internal */ _rpcWebSocketIdleTimeout: ReturnType<
    typeof setTimeout
  > | null = null;
  /** @internal
   * A number that we increment every time an active connection closes.
   * Used to determine whether the same socket connection that was open
   * when an async operation started is the same one that's active when
   * its continuation fires.
   *
   */ private _rpcWebSocketGeneration: number = 0;

  /** @internal */ _disableBlockhashCaching: boolean = false;
  /** @internal */ _pollingBlockhash: boolean = false;
  /** @internal */ _blockhashInfo: {
    latestBlockhash: BlockhashWithExpiryBlockHeight | null;
    lastFetch: number;
    simulatedSignatures: Array<string>;
    transactionSignatures: Array<string>;
  } = {
    latestBlockhash: null,
    lastFetch: 0,
    transactionSignatures: [],
    simulatedSignatures: [],
  };

  /** @internal */ private _nextClientSubscriptionId: ClientSubscriptionId = 0;
  /** @internal */ private _subscriptionDisposeFunctionsByClientSubscriptionId: {
    [clientSubscriptionId: ClientSubscriptionId]:
      | SubscriptionDisposeFn
      | undefined;
  } = {};
  /** @internal */ private _subscriptionCallbacksByServerSubscriptionId: {
    [serverSubscriptionId: ServerSubscriptionId]:
      | Set<SubscriptionConfig['callback']>
      | undefined;
  } = {};
  /** @internal */ private _subscriptionsByHash: {
    [hash: SubscriptionConfigHash]: Subscription | undefined;
  } = {};
  /**
   * Special case.
   * After a signature is processed, RPCs automatically dispose of the
   * subscription on the server side. We need to track which of these
   * subscriptions have been disposed in such a way, so that we know
   * whether the client is dealing with a not-yet-processed signature
   * (in which case we must tear down the server subscription) or an
   * already-processed signature (in which case the client can simply
   * clear out the subscription locally without telling the server).
   *
   * NOTE: There is a proposal to eliminate this special case, here:
   * https://github.com/solana-labs/solana/issues/18892
   */
  /** @internal */ private _subscriptionsAutoDisposedByRpc: Set<ServerSubscriptionId> =
    new Set();

  /**
   * Establish a JSON RPC connection
   *
   * @param endpoint URL to the fullnode JSON RPC endpoint
   * @param commitmentOrConfig optional default commitment level or optional ConnectionConfig configuration object
   */
  constructor(
    endpoint: string,
    commitmentOrConfig?: Commitment | ConnectionConfig,
  ) {
    let url = new URL(endpoint);
    const useHttps = url.protocol === 'https:';

    let wsEndpoint;
    let httpHeaders;
    let fetch;
    let fetchMiddleware;
    let disableRetryOnRateLimit;
    if (commitmentOrConfig && typeof commitmentOrConfig === 'string') {
      this._commitment = commitmentOrConfig;
    } else if (commitmentOrConfig) {
      this._commitment = commitmentOrConfig.commitment;
      this._confirmTransactionInitialTimeout =
        commitmentOrConfig.confirmTransactionInitialTimeout;
      wsEndpoint = commitmentOrConfig.wsEndpoint;
      httpHeaders = commitmentOrConfig.httpHeaders;
      fetch = commitmentOrConfig.fetch;
      fetchMiddleware = commitmentOrConfig.fetchMiddleware;
      disableRetryOnRateLimit = commitmentOrConfig.disableRetryOnRateLimit;
    }

    this._rpcEndpoint = endpoint;
    this._rpcWsEndpoint = wsEndpoint || makeWebsocketUrl(endpoint);

    this._rpcClient = createRpcClient(
      url.toString(),
      useHttps,
      httpHeaders,
      fetch,
      fetchMiddleware,
      disableRetryOnRateLimit,
    );
    this._rpcRequest = createRpcRequest(this._rpcClient);
    this._rpcBatchRequest = createRpcBatchRequest(this._rpcClient);

    this._rpcWebSocket = new RpcWebSocketClient(this._rpcWsEndpoint, {
      autoconnect: false,
      max_reconnects: Infinity,
    });
    this._rpcWebSocket.on('open', this._wsOnOpen.bind(this));
    this._rpcWebSocket.on('error', this._wsOnError.bind(this));
    this._rpcWebSocket.on('close', this._wsOnClose.bind(this));
    this._rpcWebSocket.on(
      'accountNotification',
      this._wsOnAccountNotification.bind(this),
    );
    this._rpcWebSocket.on(
      'programNotification',
      this._wsOnProgramAccountNotification.bind(this),
    );
    this._rpcWebSocket.on(
      'slotNotification',
      this._wsOnSlotNotification.bind(this),
    );
    this._rpcWebSocket.on(
      'slotsUpdatesNotification',
      this._wsOnSlotUpdatesNotification.bind(this),
    );
    this._rpcWebSocket.on(
      'signatureNotification',
      this._wsOnSignatureNotification.bind(this),
    );
    this._rpcWebSocket.on(
      'rootNotification',
      this._wsOnRootNotification.bind(this),
    );
    this._rpcWebSocket.on(
      'logsNotification',
      this._wsOnLogsNotification.bind(this),
    );
  }

  /**
   * The default commitment used for requests
   */
  get commitment(): Commitment | undefined {
    return this._commitment;
  }

  /**
   * The RPC endpoint
   */
  get rpcEndpoint(): string {
    return this._rpcEndpoint;
  }

  /**
   * Fetch the balance for the specified public key, return with context
   */
  async getBalanceAndContext(
    publicKey: PublicKey,
    commitmentOrConfig?: Commitment | GetBalanceConfig,
  ): Promise<RpcResponseAndContext<number>> {
    /** @internal */
    const {commitment, config} =
      extractCommitmentFromConfig(commitmentOrConfig);
    const args = this._buildArgs(
      [publicKey.toBase58()],
      commitment,
      undefined /* encoding */,
      config,
    );
    const unsafeRes = await this._rpcRequest('getBalance', args);
    const res = create(unsafeRes, jsonRpcResultAndContext(number()));
    if ('error' in res) {
      throw new Error(
        'failed to get balance for ' +
          publicKey.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch the balance for the specified public key
   */
  async getBalance(
    publicKey: PublicKey,
    commitmentOrConfig?: Commitment | GetBalanceConfig,
  ): Promise<number> {
    return await this.getBalanceAndContext(publicKey, commitmentOrConfig)
      .then(x => x.value)
      .catch(e => {
        throw new Error(
          'failed to get balance of account ' + publicKey.toBase58() + ': ' + e,
        );
      });
  }

  /**
   * Fetch the estimated production time of a block
   */
  async getBlockTime(slot: number): Promise<number | null> {
    const unsafeRes = await this._rpcRequest('getBlockTime', [slot]);
    const res = create(unsafeRes, jsonRpcResult(nullable(number())));
    if ('error' in res) {
      throw new Error(
        'failed to get block time for slot ' + slot + ': ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch the lowest slot that the node has information about in its ledger.
   * This value may increase over time if the node is configured to purge older ledger data
   */
  async getMinimumLedgerSlot(): Promise<number> {
    const unsafeRes = await this._rpcRequest('minimumLedgerSlot', []);
    const res = create(unsafeRes, jsonRpcResult(number()));
    if ('error' in res) {
      throw new Error(
        'failed to get minimum ledger slot: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch the slot of the lowest confirmed block that has not been purged from the ledger
   */
  async getFirstAvailableBlock(): Promise<number> {
    const unsafeRes = await this._rpcRequest('getFirstAvailableBlock', []);
    const res = create(unsafeRes, SlotRpcResult);
    if ('error' in res) {
      throw new Error(
        'failed to get first available block: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch information about the current supply
   */
  async getSupply(
    config?: GetSupplyConfig | Commitment,
  ): Promise<RpcResponseAndContext<Supply>> {
    let configArg: GetSupplyConfig = {};
    if (typeof config === 'string') {
      configArg = {commitment: config};
    } else if (config) {
      configArg = {
        ...config,
        commitment: (config && config.commitment) || this.commitment,
      };
    } else {
      configArg = {
        commitment: this.commitment,
      };
    }

    const unsafeRes = await this._rpcRequest('getSupply', [configArg]);
    const res = create(unsafeRes, GetSupplyRpcResult);
    if ('error' in res) {
      throw new Error('failed to get supply: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current supply of a token mint
   */
  async getTokenSupply(
    tokenMintAddress: PublicKey,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<TokenAmount>> {
    const args = this._buildArgs([tokenMintAddress.toBase58()], commitment);
    const unsafeRes = await this._rpcRequest('getTokenSupply', args);
    const res = create(unsafeRes, jsonRpcResultAndContext(TokenAmountResult));
    if ('error' in res) {
      throw new Error('failed to get token supply: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current balance of a token account
   */
  async getTokenAccountBalance(
    tokenAddress: PublicKey,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<TokenAmount>> {
    const args = this._buildArgs([tokenAddress.toBase58()], commitment);
    const unsafeRes = await this._rpcRequest('getTokenAccountBalance', args);
    const res = create(unsafeRes, jsonRpcResultAndContext(TokenAmountResult));
    if ('error' in res) {
      throw new Error(
        'failed to get token account balance: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch all the token accounts owned by the specified account
   *
   * @return {Promise<RpcResponseAndContext<Array<{pubkey: PublicKey, account: AccountInfo<Buffer>}>>>}
   */
  async getTokenAccountsByOwner(
    ownerAddress: PublicKey,
    filter: TokenAccountsFilter,
    commitment?: Commitment,
  ): Promise<
    RpcResponseAndContext<
      Array<{pubkey: PublicKey; account: AccountInfo<Buffer>}>
    >
  > {
    let _args: any[] = [ownerAddress.toBase58()];
    if ('mint' in filter) {
      _args.push({mint: filter.mint.toBase58()});
    } else {
      _args.push({programId: filter.programId.toBase58()});
    }

    const args = this._buildArgs(_args, commitment, 'base64');
    const unsafeRes = await this._rpcRequest('getTokenAccountsByOwner', args);
    const res = create(unsafeRes, GetTokenAccountsByOwner);
    if ('error' in res) {
      throw new Error(
        'failed to get token accounts owned by account ' +
          ownerAddress.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch parsed token accounts owned by the specified account
   *
   * @return {Promise<RpcResponseAndContext<Array<{pubkey: PublicKey, account: AccountInfo<ParsedAccountData>}>>>}
   */
  async getParsedTokenAccountsByOwner(
    ownerAddress: PublicKey,
    filter: TokenAccountsFilter,
    commitment?: Commitment,
  ): Promise<
    RpcResponseAndContext<
      Array<{pubkey: PublicKey; account: AccountInfo<ParsedAccountData>}>
    >
  > {
    let _args: any[] = [ownerAddress.toBase58()];
    if ('mint' in filter) {
      _args.push({mint: filter.mint.toBase58()});
    } else {
      _args.push({programId: filter.programId.toBase58()});
    }

    const args = this._buildArgs(_args, commitment, 'jsonParsed');
    const unsafeRes = await this._rpcRequest('getTokenAccountsByOwner', args);
    const res = create(unsafeRes, GetParsedTokenAccountsByOwner);
    if ('error' in res) {
      throw new Error(
        'failed to get token accounts owned by account ' +
          ownerAddress.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch the 20 largest accounts with their current balances
   */
  async getLargestAccounts(
    config?: GetLargestAccountsConfig,
  ): Promise<RpcResponseAndContext<Array<AccountBalancePair>>> {
    const arg = {
      ...config,
      commitment: (config && config.commitment) || this.commitment,
    };
    const args = arg.filter || arg.commitment ? [arg] : [];
    const unsafeRes = await this._rpcRequest('getLargestAccounts', args);
    const res = create(unsafeRes, GetLargestAccountsRpcResult);
    if ('error' in res) {
      throw new Error('failed to get largest accounts: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the 20 largest token accounts with their current balances
   * for a given mint.
   */
  async getTokenLargestAccounts(
    mintAddress: PublicKey,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<Array<TokenAccountBalancePair>>> {
    const args = this._buildArgs([mintAddress.toBase58()], commitment);
    const unsafeRes = await this._rpcRequest('getTokenLargestAccounts', args);
    const res = create(unsafeRes, GetTokenLargestAccountsResult);
    if ('error' in res) {
      throw new Error(
        'failed to get token largest accounts: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch all the account info for the specified public key, return with context
   */
  async getAccountInfoAndContext(
    publicKey: PublicKey,
    commitmentOrConfig?: Commitment | GetAccountInfoConfig,
  ): Promise<RpcResponseAndContext<AccountInfo<Buffer> | null>> {
    const {commitment, config} =
      extractCommitmentFromConfig(commitmentOrConfig);
    const args = this._buildArgs(
      [publicKey.toBase58()],
      commitment,
      'base64',
      config,
    );
    const unsafeRes = await this._rpcRequest('getAccountInfo', args);
    const res = create(
      unsafeRes,
      jsonRpcResultAndContext(nullable(AccountInfoResult)),
    );
    if ('error' in res) {
      throw new Error(
        'failed to get info about account ' +
          publicKey.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch parsed account info for the specified public key
   */
  async getParsedAccountInfo(
    publicKey: PublicKey,
    commitment?: Commitment,
  ): Promise<
    RpcResponseAndContext<AccountInfo<Buffer | ParsedAccountData> | null>
  > {
    const args = this._buildArgs(
      [publicKey.toBase58()],
      commitment,
      'jsonParsed',
    );
    const unsafeRes = await this._rpcRequest('getAccountInfo', args);
    const res = create(
      unsafeRes,
      jsonRpcResultAndContext(nullable(ParsedAccountInfoResult)),
    );
    if ('error' in res) {
      throw new Error(
        'failed to get info about account ' +
          publicKey.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch all the account info for the specified public key
   */
  async getAccountInfo(
    publicKey: PublicKey,
    commitmentOrConfig?: Commitment | GetAccountInfoConfig,
  ): Promise<AccountInfo<Buffer> | null> {
    try {
      const res = await this.getAccountInfoAndContext(
        publicKey,
        commitmentOrConfig,
      );
      return res.value;
    } catch (e) {
      throw new Error(
        'failed to get info about account ' + publicKey.toBase58() + ': ' + e,
      );
    }
  }

  /**
   * Fetch all the account info for multiple accounts specified by an array of public keys, return with context
   */
  async getMultipleAccountsInfoAndContext(
    publicKeys: PublicKey[],
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<(AccountInfo<Buffer> | null)[]>> {
    const keys = publicKeys.map(key => key.toBase58());
    const args = this._buildArgs([keys], commitment, 'base64');
    const unsafeRes = await this._rpcRequest('getMultipleAccounts', args);
    const res = create(
      unsafeRes,
      jsonRpcResultAndContext(array(nullable(AccountInfoResult))),
    );
    if ('error' in res) {
      throw new Error(
        'failed to get info for accounts ' + keys + ': ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch all the account info for multiple accounts specified by an array of public keys
   */
  async getMultipleAccountsInfo(
    publicKeys: PublicKey[],
    commitment?: Commitment,
  ): Promise<(AccountInfo<Buffer> | null)[]> {
    const res = await this.getMultipleAccountsInfoAndContext(
      publicKeys,
      commitment,
    );
    return res.value;
  }

  /**
   * Returns epoch activation information for a stake account that has been delegated
   */
  async getStakeActivation(
    publicKey: PublicKey,
    commitment?: Commitment,
    epoch?: number,
  ): Promise<StakeActivationData> {
    const args = this._buildArgs(
      [publicKey.toBase58()],
      commitment,
      undefined,
      epoch !== undefined ? {epoch} : undefined,
    );

    const unsafeRes = await this._rpcRequest('getStakeActivation', args);
    const res = create(unsafeRes, jsonRpcResult(StakeActivationResult));
    if ('error' in res) {
      throw new Error(
        `failed to get Stake Activation ${publicKey.toBase58()}: ${
          res.error.message
        }`,
      );
    }
    return res.result;
  }

  /**
   * Fetch all the accounts owned by the specified program id
   *
   * @return {Promise<Array<{pubkey: PublicKey, account: AccountInfo<Buffer>}>>}
   */
  async getProgramAccounts(
    programId: PublicKey,
    configOrCommitment?: GetProgramAccountsConfig | Commitment,
  ): Promise<Array<{pubkey: PublicKey; account: AccountInfo<Buffer>}>> {
    const extra: Pick<GetProgramAccountsConfig, 'dataSlice' | 'filters'> = {};

    let commitment;
    let encoding;
    if (configOrCommitment) {
      if (typeof configOrCommitment === 'string') {
        commitment = configOrCommitment;
      } else {
        commitment = configOrCommitment.commitment;
        encoding = configOrCommitment.encoding;

        if (configOrCommitment.dataSlice) {
          extra.dataSlice = configOrCommitment.dataSlice;
        }
        if (configOrCommitment.filters) {
          extra.filters = configOrCommitment.filters;
        }
      }
    }

    const args = this._buildArgs(
      [programId.toBase58()],
      commitment,
      encoding || 'base64',
      extra,
    );
    const unsafeRes = await this._rpcRequest('getProgramAccounts', args);
    const res = create(unsafeRes, jsonRpcResult(array(KeyedAccountInfoResult)));
    if ('error' in res) {
      throw new Error(
        'failed to get accounts owned by program ' +
          programId.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch and parse all the accounts owned by the specified program id
   *
   * @return {Promise<Array<{pubkey: PublicKey, account: AccountInfo<Buffer | ParsedAccountData>}>>}
   */
  async getParsedProgramAccounts(
    programId: PublicKey,
    configOrCommitment?: GetParsedProgramAccountsConfig | Commitment,
  ): Promise<
    Array<{
      pubkey: PublicKey;
      account: AccountInfo<Buffer | ParsedAccountData>;
    }>
  > {
    const extra: Pick<GetParsedProgramAccountsConfig, 'filters'> = {};

    let commitment;
    if (configOrCommitment) {
      if (typeof configOrCommitment === 'string') {
        commitment = configOrCommitment;
      } else {
        commitment = configOrCommitment.commitment;

        if (configOrCommitment.filters) {
          extra.filters = configOrCommitment.filters;
        }
      }
    }

    const args = this._buildArgs(
      [programId.toBase58()],
      commitment,
      'jsonParsed',
      extra,
    );
    const unsafeRes = await this._rpcRequest('getProgramAccounts', args);
    const res = create(
      unsafeRes,
      jsonRpcResult(array(KeyedParsedAccountInfoResult)),
    );
    if ('error' in res) {
      throw new Error(
        'failed to get accounts owned by program ' +
          programId.toBase58() +
          ': ' +
          res.error.message,
      );
    }
    return res.result;
  }

  confirmTransaction(
    strategy: BlockheightBasedTransactionConfirmationStrategy,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<SignatureResult>>;

  /** @deprecated Instead, call `confirmTransaction` using a `TransactionConfirmationConfig` */
  // eslint-disable-next-line no-dupe-class-members
  confirmTransaction(
    strategy: TransactionSignature,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<SignatureResult>>;

  // eslint-disable-next-line no-dupe-class-members
  async confirmTransaction(
    strategy:
      | BlockheightBasedTransactionConfirmationStrategy
      | TransactionSignature,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<SignatureResult>> {
    let rawSignature: string;

    if (typeof strategy == 'string') {
      rawSignature = strategy;
    } else {
      const config =
        strategy as BlockheightBasedTransactionConfirmationStrategy;
      rawSignature = config.signature;
    }

    let decodedSignature;

    try {
      decodedSignature = bs58.decode(rawSignature);
    } catch (err) {
      throw new Error('signature must be base58 encoded: ' + rawSignature);
    }

    assert(decodedSignature.length === 64, 'signature has invalid length');

    const subscriptionCommitment = commitment || this.commitment;
    let timeoutId;
    let subscriptionId;
    let done = false;

    const confirmationPromise = new Promise<{
      __type: TransactionStatus.PROCESSED;
      response: RpcResponseAndContext<SignatureResult>;
    }>((resolve, reject) => {
      try {
        subscriptionId = this.onSignature(
          rawSignature,
          (result: SignatureResult, context: Context) => {
            subscriptionId = undefined;
            const response = {
              context,
              value: result,
            };
            done = true;
            resolve({__type: TransactionStatus.PROCESSED, response});
          },
          subscriptionCommitment,
        );
      } catch (err) {
        reject(err);
      }
    });

    const checkBlockHeight = async () => {
      try {
        const blockHeight = await this.getBlockHeight(commitment);
        return blockHeight;
      } catch (_e) {
        return -1;
      }
    };

    const expiryPromise = new Promise<
      | {__type: TransactionStatus.BLOCKHEIGHT_EXCEEDED}
      | {__type: TransactionStatus.TIMED_OUT; timeoutMs: number}
    >(resolve => {
      if (typeof strategy === 'string') {
        let timeoutMs = this._confirmTransactionInitialTimeout || 60 * 1000;
        switch (subscriptionCommitment) {
          case 'processed':
          case 'recent':
          case 'single':
          case 'confirmed':
          case 'singleGossip': {
            timeoutMs = this._confirmTransactionInitialTimeout || 30 * 1000;
            break;
          }
          // exhaust enums to ensure full coverage
          case 'finalized':
          case 'max':
          case 'root':
        }

        timeoutId = setTimeout(
          () => resolve({__type: TransactionStatus.TIMED_OUT, timeoutMs}),
          timeoutMs,
        );
      } else {
        let config =
          strategy as BlockheightBasedTransactionConfirmationStrategy;
        (async () => {
          let currentBlockHeight = await checkBlockHeight();
          if (done) return;
          while (currentBlockHeight <= config.lastValidBlockHeight) {
            await sleep(1000);
            if (done) return;
            currentBlockHeight = await checkBlockHeight();
            if (done) return;
          }
          resolve({__type: TransactionStatus.BLOCKHEIGHT_EXCEEDED});
        })();
      }
    });

    let result: RpcResponseAndContext<SignatureResult>;
    try {
      const outcome = await Promise.race([confirmationPromise, expiryPromise]);
      switch (outcome.__type) {
        case TransactionStatus.BLOCKHEIGHT_EXCEEDED:
          throw new TransactionExpiredBlockheightExceededError(rawSignature);
        case TransactionStatus.PROCESSED:
          result = outcome.response;
          break;
        case TransactionStatus.TIMED_OUT:
          throw new TransactionExpiredTimeoutError(
            rawSignature,
            outcome.timeoutMs / 1000,
          );
      }
    } finally {
      clearTimeout(timeoutId);
      if (subscriptionId) {
        this.removeSignatureListener(subscriptionId);
      }
    }
    return result;
  }

  /**
   * Return the list of nodes that are currently participating in the cluster
   */
  async getClusterNodes(): Promise<Array<ContactInfo>> {
    const unsafeRes = await this._rpcRequest('getClusterNodes', []);
    const res = create(unsafeRes, jsonRpcResult(array(ContactInfoResult)));
    if ('error' in res) {
      throw new Error('failed to get cluster nodes: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Return the list of nodes that are currently participating in the cluster
   */
  async getVoteAccounts(commitment?: Commitment): Promise<VoteAccountStatus> {
    const args = this._buildArgs([], commitment);
    const unsafeRes = await this._rpcRequest('getVoteAccounts', args);
    const res = create(unsafeRes, GetVoteAccounts);
    if ('error' in res) {
      throw new Error('failed to get vote accounts: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current slot that the node is processing
   */
  async getSlot(commitment?: Commitment): Promise<number> {
    const args = this._buildArgs([], commitment);
    const unsafeRes = await this._rpcRequest('getSlot', args);
    const res = create(unsafeRes, jsonRpcResult(number()));
    if ('error' in res) {
      throw new Error('failed to get slot: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current slot leader of the cluster
   */
  async getSlotLeader(commitment?: Commitment): Promise<string> {
    const args = this._buildArgs([], commitment);
    const unsafeRes = await this._rpcRequest('getSlotLeader', args);
    const res = create(unsafeRes, jsonRpcResult(string()));
    if ('error' in res) {
      throw new Error('failed to get slot leader: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch `limit` number of slot leaders starting from `startSlot`
   *
   * @param startSlot fetch slot leaders starting from this slot
   * @param limit number of slot leaders to return
   */
  async getSlotLeaders(
    startSlot: number,
    limit: number,
  ): Promise<Array<PublicKey>> {
    const args = [startSlot, limit];
    const unsafeRes = await this._rpcRequest('getSlotLeaders', args);
    const res = create(unsafeRes, jsonRpcResult(array(PublicKeyFromString)));
    if ('error' in res) {
      throw new Error('failed to get slot leaders: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current status of a signature
   */
  async getSignatureStatus(
    signature: TransactionSignature,
    config?: SignatureStatusConfig,
  ): Promise<RpcResponseAndContext<SignatureStatus | null>> {
    const {context, value: values} = await this.getSignatureStatuses(
      [signature],
      config,
    );
    assert(values.length === 1);
    const value = values[0];
    return {context, value};
  }

  /**
   * Fetch the current statuses of a batch of signatures
   */
  async getSignatureStatuses(
    signatures: Array<TransactionSignature>,
    config?: SignatureStatusConfig,
  ): Promise<RpcResponseAndContext<Array<SignatureStatus | null>>> {
    const params: any[] = [signatures];
    if (config) {
      params.push(config);
    }
    const unsafeRes = await this._rpcRequest('getSignatureStatuses', params);
    const res = create(unsafeRes, GetSignatureStatusesRpcResult);
    if ('error' in res) {
      throw new Error('failed to get signature status: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current transaction count of the cluster
   */
  async getTransactionCount(commitment?: Commitment): Promise<number> {
    const args = this._buildArgs([], commitment);
    const unsafeRes = await this._rpcRequest('getTransactionCount', args);
    const res = create(unsafeRes, jsonRpcResult(number()));
    if ('error' in res) {
      throw new Error('failed to get transaction count: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the current total currency supply of the cluster in lamports
   *
   * @deprecated Deprecated since v1.2.8. Please use {@link getSupply} instead.
   */
  async getTotalSupply(commitment?: Commitment): Promise<number> {
    const result = await this.getSupply({
      commitment,
      excludeNonCirculatingAccountsList: true,
    });
    return result.value.total;
  }

  /**
   * Fetch the cluster InflationGovernor parameters
   */
  async getInflationGovernor(
    commitment?: Commitment,
  ): Promise<InflationGovernor> {
    const args = this._buildArgs([], commitment);
    const unsafeRes = await this._rpcRequest('getInflationGovernor', args);
    const res = create(unsafeRes, GetInflationGovernorRpcResult);
    if ('error' in res) {
      throw new Error('failed to get inflation: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the inflation reward for a list of addresses for an epoch
   */
  async getInflationReward(
    addresses: PublicKey[],
    epoch?: number,
    commitmentOrConfig?: Commitment | GetInflationRewardConfig,
  ): Promise<(InflationReward | null)[]> {
    const {commitment, config} =
      extractCommitmentFromConfig(commitmentOrConfig);
    const args = this._buildArgs(
      [addresses.map(pubkey => pubkey.toBase58())],
      commitment,
      undefined /* encoding */,
      {
        ...config,
        epoch: epoch != null ? epoch : config?.epoch,
      },
    );
    const unsafeRes = await this._rpcRequest('getInflationReward', args);
    const res = create(unsafeRes, GetInflationRewardResult);
    if ('error' in res) {
      throw new Error('failed to get inflation reward: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the Epoch Info parameters
   */
  async getEpochInfo(
    commitmentOrConfig?: Commitment | GetEpochInfoConfig,
  ): Promise<EpochInfo> {
    const {commitment, config} =
      extractCommitmentFromConfig(commitmentOrConfig);
    const args = this._buildArgs(
      [],
      commitment,
      undefined /* encoding */,
      config,
    );
    const unsafeRes = await this._rpcRequest('getEpochInfo', args);
    const res = create(unsafeRes, GetEpochInfoRpcResult);
    if ('error' in res) {
      throw new Error('failed to get epoch info: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the Epoch Schedule parameters
   */
  async getEpochSchedule(): Promise<EpochSchedule> {
    const unsafeRes = await this._rpcRequest('getEpochSchedule', []);
    const res = create(unsafeRes, GetEpochScheduleRpcResult);
    if ('error' in res) {
      throw new Error('failed to get epoch schedule: ' + res.error.message);
    }
    const epochSchedule = res.result;
    return new EpochSchedule(
      epochSchedule.slotsPerEpoch,
      epochSchedule.leaderScheduleSlotOffset,
      epochSchedule.warmup,
      epochSchedule.firstNormalEpoch,
      epochSchedule.firstNormalSlot,
    );
  }

  /**
   * Fetch the leader schedule for the current epoch
   * @return {Promise<RpcResponseAndContext<LeaderSchedule>>}
   */
  async getLeaderSchedule(): Promise<LeaderSchedule> {
    const unsafeRes = await this._rpcRequest('getLeaderSchedule', []);
    const res = create(unsafeRes, GetLeaderScheduleRpcResult);
    if ('error' in res) {
      throw new Error('failed to get leader schedule: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the minimum balance needed to exempt an account of `dataLength`
   * size from rent
   */
  async getMinimumBalanceForRentExemption(
    dataLength: number,
    commitment?: Commitment,
  ): Promise<number> {
    const args = this._buildArgs([dataLength], commitment);
    const unsafeRes = await this._rpcRequest(
      'getMinimumBalanceForRentExemption',
      args,
    );
    const res = create(unsafeRes, GetMinimumBalanceForRentExemptionRpcResult);
    if ('error' in res) {
      console.warn('Unable to fetch minimum balance for rent exemption');
      return 0;
    }
    return res.result;
  }

  /**
   * Fetch a recent blockhash from the cluster, return with context
   * @return {Promise<RpcResponseAndContext<{blockhash: Blockhash, feeCalculator: FeeCalculator}>>}
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getLatestBlockhash} instead.
   */
  async getRecentBlockhashAndContext(
    commitment?: Commitment,
  ): Promise<
    RpcResponseAndContext<{blockhash: Blockhash; feeCalculator: FeeCalculator}>
  > {
    const args = this._buildArgs([], commitment);
    const unsafeRes = await this._rpcRequest('getRecentBlockhash', args);
    const res = create(unsafeRes, GetRecentBlockhashAndContextRpcResult);
    if ('error' in res) {
      throw new Error('failed to get recent blockhash: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch recent performance samples
   * @return {Promise<Array<PerfSample>>}
   */
  async getRecentPerformanceSamples(
    limit?: number,
  ): Promise<Array<PerfSample>> {
    const args = this._buildArgs(limit ? [limit] : []);
    const unsafeRes = await this._rpcRequest(
      'getRecentPerformanceSamples',
      args,
    );
    const res = create(unsafeRes, GetRecentPerformanceSamplesRpcResult);
    if ('error' in res) {
      throw new Error(
        'failed to get recent performance samples: ' + res.error.message,
      );
    }

    return res.result;
  }

  /**
   * Fetch the fee calculator for a recent blockhash from the cluster, return with context
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getFeeForMessage} instead.
   */
  async getFeeCalculatorForBlockhash(
    blockhash: Blockhash,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<FeeCalculator | null>> {
    const args = this._buildArgs([blockhash], commitment);
    const unsafeRes = await this._rpcRequest(
      'getFeeCalculatorForBlockhash',
      args,
    );

    const res = create(unsafeRes, GetFeeCalculatorRpcResult);
    if ('error' in res) {
      throw new Error('failed to get fee calculator: ' + res.error.message);
    }
    const {context, value} = res.result;
    return {
      context,
      value: value !== null ? value.feeCalculator : null,
    };
  }

  /**
   * Fetch the fee for a message from the cluster, return with context
   */
  async getFeeForMessage(
    message: Message,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<number>> {
    const wireMessage = message.serialize().toString('base64');
    const args = this._buildArgs([wireMessage], commitment);
    const unsafeRes = await this._rpcRequest('getFeeForMessage', args);

    const res = create(unsafeRes, jsonRpcResultAndContext(nullable(number())));
    if ('error' in res) {
      throw new Error('failed to get slot: ' + res.error.message);
    }
    if (res.result === null) {
      throw new Error('invalid blockhash');
    }
    return res.result as unknown as RpcResponseAndContext<number>;
  }

  /**
   * Fetch a recent blockhash from the cluster
   * @return {Promise<{blockhash: Blockhash, feeCalculator: FeeCalculator}>}
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getLatestBlockhash} instead.
   */
  async getRecentBlockhash(
    commitment?: Commitment,
  ): Promise<{blockhash: Blockhash; feeCalculator: FeeCalculator}> {
    try {
      const res = await this.getRecentBlockhashAndContext(commitment);
      return res.value;
    } catch (e) {
      throw new Error('failed to get recent blockhash: ' + e);
    }
  }

  /**
   * Fetch the latest blockhash from the cluster
   * @return {Promise<BlockhashWithExpiryBlockHeight>}
   */
  async getLatestBlockhash(
    commitmentOrConfig?: Commitment | GetLatestBlockhashConfig,
  ): Promise<BlockhashWithExpiryBlockHeight> {
    try {
      const res = await this.getLatestBlockhashAndContext(commitmentOrConfig);
      return res.value;
    } catch (e) {
      throw new Error('failed to get recent blockhash: ' + e);
    }
  }

  /**
   * Fetch the latest blockhash from the cluster
   * @return {Promise<BlockhashWithExpiryBlockHeight>}
   */
  async getLatestBlockhashAndContext(
    commitmentOrConfig?: Commitment | GetLatestBlockhashConfig,
  ): Promise<RpcResponseAndContext<BlockhashWithExpiryBlockHeight>> {
    const {commitment, config} =
      extractCommitmentFromConfig(commitmentOrConfig);
    const args = this._buildArgs(
      [],
      commitment,
      undefined /* encoding */,
      config,
    );
    const unsafeRes = await this._rpcRequest('getLatestBlockhash', args);
    const res = create(unsafeRes, GetLatestBlockhashRpcResult);
    if ('error' in res) {
      throw new Error('failed to get latest blockhash: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the node version
   */
  async getVersion(): Promise<Version> {
    const unsafeRes = await this._rpcRequest('getVersion', []);
    const res = create(unsafeRes, jsonRpcResult(VersionResult));
    if ('error' in res) {
      throw new Error('failed to get version: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch the genesis hash
   */
  async getGenesisHash(): Promise<string> {
    const unsafeRes = await this._rpcRequest('getGenesisHash', []);
    const res = create(unsafeRes, jsonRpcResult(string()));
    if ('error' in res) {
      throw new Error('failed to get genesis hash: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch a processed block from the cluster.
   */
  async getBlock(
    slot: number,
    opts?: {commitment?: Finality},
  ): Promise<BlockResponse | null> {
    const args = this._buildArgsAtLeastConfirmed(
      [slot],
      opts && opts.commitment,
    );
    const unsafeRes = await this._rpcRequest('getBlock', args);
    const res = create(unsafeRes, GetBlockRpcResult);

    if ('error' in res) {
      throw new Error('failed to get confirmed block: ' + res.error.message);
    }

    const result = res.result;
    if (!result) return result;

    return {
      ...result,
      transactions: result.transactions.map(({transaction, meta}) => {
        const message = new Message(transaction.message);
        return {
          meta,
          transaction: {
            ...transaction,
            message,
          },
        };
      }),
    };
  }

  /*
   * Returns the current block height of the node
   */
  async getBlockHeight(
    commitmentOrConfig?: Commitment | GetBlockHeightConfig,
  ): Promise<number> {
    const {commitment, config} =
      extractCommitmentFromConfig(commitmentOrConfig);
    const args = this._buildArgs(
      [],
      commitment,
      undefined /* encoding */,
      config,
    );
    const unsafeRes = await this._rpcRequest('getBlockHeight', args);
    const res = create(unsafeRes, jsonRpcResult(number()));
    if ('error' in res) {
      throw new Error(
        'failed to get block height information: ' + res.error.message,
      );
    }

    return res.result;
  }

  /*
   * Returns recent block production information from the current or previous epoch
   */
  async getBlockProduction(
    configOrCommitment?: GetBlockProductionConfig | Commitment,
  ): Promise<RpcResponseAndContext<BlockProduction>> {
    let extra: Omit<GetBlockProductionConfig, 'commitment'> | undefined;
    let commitment: Commitment | undefined;

    if (typeof configOrCommitment === 'string') {
      commitment = configOrCommitment;
    } else if (configOrCommitment) {
      const {commitment: c, ...rest} = configOrCommitment;
      commitment = c;
      extra = rest;
    }

    const args = this._buildArgs([], commitment, 'base64', extra);
    const unsafeRes = await this._rpcRequest('getBlockProduction', args);
    const res = create(unsafeRes, BlockProductionResponseStruct);
    if ('error' in res) {
      throw new Error(
        'failed to get block production information: ' + res.error.message,
      );
    }

    return res.result;
  }

  /**
   * Fetch a confirmed or finalized transaction from the cluster.
   */
  async getTransaction(
    signature: string,
    opts?: {commitment?: Finality},
  ): Promise<TransactionResponse | null> {
    const args = this._buildArgsAtLeastConfirmed(
      [signature],
      opts && opts.commitment,
    );
    const unsafeRes = await this._rpcRequest('getTransaction', args);
    const res = create(unsafeRes, GetTransactionRpcResult);
    if ('error' in res) {
      throw new Error('failed to get transaction: ' + res.error.message);
    }

    const result = res.result;
    if (!result) return result;

    return {
      ...result,
      transaction: {
        ...result.transaction,
        message: new Message(result.transaction.message),
      },
    };
  }

  /**
   * Fetch parsed transaction details for a confirmed or finalized transaction
   */
  async getParsedTransaction(
    signature: TransactionSignature,
    commitment?: Finality,
  ): Promise<ParsedConfirmedTransaction | null> {
    const args = this._buildArgsAtLeastConfirmed(
      [signature],
      commitment,
      'jsonParsed',
    );
    const unsafeRes = await this._rpcRequest('getTransaction', args);
    const res = create(unsafeRes, GetParsedTransactionRpcResult);
    if ('error' in res) {
      throw new Error('failed to get transaction: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch parsed transaction details for a batch of confirmed transactions
   */
  async getParsedTransactions(
    signatures: TransactionSignature[],
    commitment?: Finality,
  ): Promise<(ParsedConfirmedTransaction | null)[]> {
    const batch = signatures.map(signature => {
      const args = this._buildArgsAtLeastConfirmed(
        [signature],
        commitment,
        'jsonParsed',
      );
      return {
        methodName: 'getTransaction',
        args,
      };
    });

    const unsafeRes = await this._rpcBatchRequest(batch);
    const res = unsafeRes.map((unsafeRes: any) => {
      const res = create(unsafeRes, GetParsedTransactionRpcResult);
      if ('error' in res) {
        throw new Error('failed to get transactions: ' + res.error.message);
      }
      return res.result;
    });

    return res;
  }

  /**
   * Fetch transaction details for a batch of confirmed transactions.
   * Similar to {@link getParsedTransactions} but returns a {@link TransactionResponse}.
   */
  async getTransactions(
    signatures: TransactionSignature[],
    commitment?: Finality,
  ): Promise<(TransactionResponse | null)[]> {
    const batch = signatures.map(signature => {
      const args = this._buildArgsAtLeastConfirmed([signature], commitment);
      return {
        methodName: 'getTransaction',
        args,
      };
    });

    const unsafeRes = await this._rpcBatchRequest(batch);
    const res = unsafeRes.map((unsafeRes: any) => {
      const res = create(unsafeRes, GetTransactionRpcResult);
      if ('error' in res) {
        throw new Error('failed to get transactions: ' + res.error.message);
      }
      const result = res.result;
      if (!result) return result;

      return {
        ...result,
        transaction: {
          ...result.transaction,
          message: new Message(result.transaction.message),
        },
      };
    });

    return res;
  }

  /**
   * Fetch a list of Transactions and transaction statuses from the cluster
   * for a confirmed block.
   *
   * @deprecated Deprecated since v1.13.0. Please use {@link getBlock} instead.
   */
  async getConfirmedBlock(
    slot: number,
    commitment?: Finality,
  ): Promise<ConfirmedBlock> {
    const args = this._buildArgsAtLeastConfirmed([slot], commitment);
    const unsafeRes = await this._rpcRequest('getConfirmedBlock', args);
    const res = create(unsafeRes, GetConfirmedBlockRpcResult);

    if ('error' in res) {
      throw new Error('failed to get confirmed block: ' + res.error.message);
    }

    const result = res.result;
    if (!result) {
      throw new Error('Confirmed block ' + slot + ' not found');
    }

    const block = {
      ...result,
      transactions: result.transactions.map(({transaction, meta}) => {
        const message = new Message(transaction.message);
        return {
          meta,
          transaction: {
            ...transaction,
            message,
          },
        };
      }),
    };

    return {
      ...block,
      transactions: block.transactions.map(({transaction, meta}) => {
        return {
          meta,
          transaction: Transaction.populate(
            transaction.message,
            transaction.signatures,
          ),
        };
      }),
    };
  }

  /**
   * Fetch confirmed blocks between two slots
   */
  async getBlocks(
    startSlot: number,
    endSlot?: number,
    commitment?: Finality,
  ): Promise<Array<number>> {
    const args = this._buildArgsAtLeastConfirmed(
      endSlot !== undefined ? [startSlot, endSlot] : [startSlot],
      commitment,
    );
    const unsafeRes = await this._rpcRequest('getBlocks', args);
    const res = create(unsafeRes, jsonRpcResult(array(number())));
    if ('error' in res) {
      throw new Error('failed to get blocks: ' + res.error.message);
    }
    return res.result;
  }

  /**
   * Fetch a list of Signatures from the cluster for a block, excluding rewards
   */
  async getBlockSignatures(
    slot: number,
    commitment?: Finality,
  ): Promise<BlockSignatures> {
    const args = this._buildArgsAtLeastConfirmed(
      [slot],
      commitment,
      undefined,
      {
        transactionDetails: 'signatures',
        rewards: false,
      },
    );
    const unsafeRes = await this._rpcRequest('getBlock', args);
    const res = create(unsafeRes, GetBlockSignaturesRpcResult);
    if ('error' in res) {
      throw new Error('failed to get block: ' + res.error.message);
    }
    const result = res.result;
    if (!result) {
      throw new Error('Block ' + slot + ' not found');
    }
    return result;
  }

  /**
   * Fetch a list of Signatures from the cluster for a confirmed block, excluding rewards
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getBlockSignatures} instead.
   */
  async getConfirmedBlockSignatures(
    slot: number,
    commitment?: Finality,
  ): Promise<BlockSignatures> {
    const args = this._buildArgsAtLeastConfirmed(
      [slot],
      commitment,
      undefined,
      {
        transactionDetails: 'signatures',
        rewards: false,
      },
    );
    const unsafeRes = await this._rpcRequest('getConfirmedBlock', args);
    const res = create(unsafeRes, GetBlockSignaturesRpcResult);
    if ('error' in res) {
      throw new Error('failed to get confirmed block: ' + res.error.message);
    }
    const result = res.result;
    if (!result) {
      throw new Error('Confirmed block ' + slot + ' not found');
    }
    return result;
  }

  /**
   * Fetch a transaction details for a confirmed transaction
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getTransaction} instead.
   */
  async getConfirmedTransaction(
    signature: TransactionSignature,
    commitment?: Finality,
  ): Promise<ConfirmedTransaction | null> {
    const args = this._buildArgsAtLeastConfirmed([signature], commitment);
    const unsafeRes = await this._rpcRequest('getConfirmedTransaction', args);
    const res = create(unsafeRes, GetTransactionRpcResult);
    if ('error' in res) {
      throw new Error('failed to get transaction: ' + res.error.message);
    }

    const result = res.result;
    if (!result) return result;

    const message = new Message(result.transaction.message);
    const signatures = result.transaction.signatures;
    return {
      ...result,
      transaction: Transaction.populate(message, signatures),
    };
  }

  /**
   * Fetch parsed transaction details for a confirmed transaction
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getParsedTransaction} instead.
   */
  async getParsedConfirmedTransaction(
    signature: TransactionSignature,
    commitment?: Finality,
  ): Promise<ParsedConfirmedTransaction | null> {
    const args = this._buildArgsAtLeastConfirmed(
      [signature],
      commitment,
      'jsonParsed',
    );
    const unsafeRes = await this._rpcRequest('getConfirmedTransaction', args);
    const res = create(unsafeRes, GetParsedTransactionRpcResult);
    if ('error' in res) {
      throw new Error(
        'failed to get confirmed transaction: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch parsed transaction details for a batch of confirmed transactions
   *
   * @deprecated Deprecated since Solana v1.8.0. Please use {@link getParsedTransactions} instead.
   */
  async getParsedConfirmedTransactions(
    signatures: TransactionSignature[],
    commitment?: Finality,
  ): Promise<(ParsedConfirmedTransaction | null)[]> {
    const batch = signatures.map(signature => {
      const args = this._buildArgsAtLeastConfirmed(
        [signature],
        commitment,
        'jsonParsed',
      );
      return {
        methodName: 'getConfirmedTransaction',
        args,
      };
    });

    const unsafeRes = await this._rpcBatchRequest(batch);
    const res = unsafeRes.map((unsafeRes: any) => {
      const res = create(unsafeRes, GetParsedTransactionRpcResult);
      if ('error' in res) {
        throw new Error(
          'failed to get confirmed transactions: ' + res.error.message,
        );
      }
      return res.result;
    });

    return res;
  }

  /**
   * Fetch a list of all the confirmed signatures for transactions involving an address
   * within a specified slot range. Max range allowed is 10,000 slots.
   *
   * @deprecated Deprecated since v1.3. Please use {@link getConfirmedSignaturesForAddress2} instead.
   *
   * @param address queried address
   * @param startSlot start slot, inclusive
   * @param endSlot end slot, inclusive
   */
  async getConfirmedSignaturesForAddress(
    address: PublicKey,
    startSlot: number,
    endSlot: number,
  ): Promise<Array<TransactionSignature>> {
    let options: any = {};

    let firstAvailableBlock = await this.getFirstAvailableBlock();
    while (!('until' in options)) {
      startSlot--;
      if (startSlot <= 0 || startSlot < firstAvailableBlock) {
        break;
      }

      try {
        const block = await this.getConfirmedBlockSignatures(
          startSlot,
          'finalized',
        );
        if (block.signatures.length > 0) {
          options.until =
            block.signatures[block.signatures.length - 1].toString();
        }
      } catch (err) {
        if (err instanceof Error && err.message.includes('skipped')) {
          continue;
        } else {
          throw err;
        }
      }
    }

    let highestConfirmedRoot = await this.getSlot('finalized');
    while (!('before' in options)) {
      endSlot++;
      if (endSlot > highestConfirmedRoot) {
        break;
      }

      try {
        const block = await this.getConfirmedBlockSignatures(endSlot);
        if (block.signatures.length > 0) {
          options.before =
            block.signatures[block.signatures.length - 1].toString();
        }
      } catch (err) {
        if (err instanceof Error && err.message.includes('skipped')) {
          continue;
        } else {
          throw err;
        }
      }
    }

    const confirmedSignatureInfo = await this.getConfirmedSignaturesForAddress2(
      address,
      options,
    );
    return confirmedSignatureInfo.map(info => info.signature);
  }

  /**
   * Returns confirmed signatures for transactions involving an
   * address backwards in time from the provided signature or most recent confirmed block
   *
   *
   * @param address queried address
   * @param options
   */
  async getConfirmedSignaturesForAddress2(
    address: PublicKey,
    options?: ConfirmedSignaturesForAddress2Options,
    commitment?: Finality,
  ): Promise<Array<ConfirmedSignatureInfo>> {
    const args = this._buildArgsAtLeastConfirmed(
      [address.toBase58()],
      commitment,
      undefined,
      options,
    );
    const unsafeRes = await this._rpcRequest(
      'getConfirmedSignaturesForAddress2',
      args,
    );
    const res = create(unsafeRes, GetConfirmedSignaturesForAddress2RpcResult);
    if ('error' in res) {
      throw new Error(
        'failed to get confirmed signatures for address: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Returns confirmed signatures for transactions involving an
   * address backwards in time from the provided signature or most recent confirmed block
   *
   *
   * @param address queried address
   * @param options
   */
  async getSignaturesForAddress(
    address: PublicKey,
    options?: SignaturesForAddressOptions,
    commitment?: Finality,
  ): Promise<Array<ConfirmedSignatureInfo>> {
    const args = this._buildArgsAtLeastConfirmed(
      [address.toBase58()],
      commitment,
      undefined,
      options,
    );
    const unsafeRes = await this._rpcRequest('getSignaturesForAddress', args);
    const res = create(unsafeRes, GetSignaturesForAddressRpcResult);
    if ('error' in res) {
      throw new Error(
        'failed to get signatures for address: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * Fetch the contents of a Nonce account from the cluster, return with context
   */
  async getNonceAndContext(
    nonceAccount: PublicKey,
    commitment?: Commitment,
  ): Promise<RpcResponseAndContext<NonceAccount | null>> {
    const {context, value: accountInfo} = await this.getAccountInfoAndContext(
      nonceAccount,
      commitment,
    );

    let value = null;
    if (accountInfo !== null) {
      value = NonceAccount.fromAccountData(accountInfo.data);
    }

    return {
      context,
      value,
    };
  }

  /**
   * Fetch the contents of a Nonce account from the cluster
   */
  async getNonce(
    nonceAccount: PublicKey,
    commitment?: Commitment,
  ): Promise<NonceAccount | null> {
    return await this.getNonceAndContext(nonceAccount, commitment)
      .then(x => x.value)
      .catch(e => {
        throw new Error(
          'failed to get nonce for account ' +
            nonceAccount.toBase58() +
            ': ' +
            e,
        );
      });
  }

  /**
   * Request an allocation of lamports to the specified address
   *
   * ```typescript
   * import { Connection, PublicKey, LAMPORTS_PER_SOL } from "@solana/web3.js";
   *
   * (async () => {
   *   const connection = new Connection("https://api.testnet.solana.com", "confirmed");
   *   const myAddress = new PublicKey("2nr1bHFT86W9tGnyvmYW4vcHKsQB3sVQfnddasz4kExM");
   *   const signature = await connection.requestAirdrop(myAddress, LAMPORTS_PER_SOL);
   *   await connection.confirmTransaction(signature);
   * })();
   * ```
   */
  async requestAirdrop(
    to: PublicKey,
    lamports: number,
  ): Promise<TransactionSignature> {
    const unsafeRes = await this._rpcRequest('requestAirdrop', [
      to.toBase58(),
      lamports,
    ]);
    const res = create(unsafeRes, RequestAirdropRpcResult);
    if ('error' in res) {
      throw new Error(
        'airdrop to ' + to.toBase58() + ' failed: ' + res.error.message,
      );
    }
    return res.result;
  }

  /**
   * @internal
   */
  async _blockhashWithExpiryBlockHeight(
    disableCache: boolean,
  ): Promise<BlockhashWithExpiryBlockHeight> {
    if (!disableCache) {
      // Wait for polling to finish
      while (this._pollingBlockhash) {
        await sleep(100);
      }
      const timeSinceFetch = Date.now() - this._blockhashInfo.lastFetch;
      const expired = timeSinceFetch >= BLOCKHASH_CACHE_TIMEOUT_MS;
      if (this._blockhashInfo.latestBlockhash !== null && !expired) {
        return this._blockhashInfo.latestBlockhash;
      }
    }

    return await this._pollNewBlockhash();
  }

  /**
   * @internal
   */
  async _pollNewBlockhash(): Promise<BlockhashWithExpiryBlockHeight> {
    this._pollingBlockhash = true;
    try {
      const startTime = Date.now();
      const cachedLatestBlockhash = this._blockhashInfo.latestBlockhash;
      const cachedBlockhash = cachedLatestBlockhash
        ? cachedLatestBlockhash.blockhash
        : null;
      for (let i = 0; i < 50; i++) {
        const latestBlockhash = await this.getLatestBlockhash('finalized');

        if (cachedBlockhash !== latestBlockhash.blockhash) {
          this._blockhashInfo = {
            latestBlockhash,
            lastFetch: Date.now(),
            transactionSignatures: [],
            simulatedSignatures: [],
          };
          return latestBlockhash;
        }

        // Sleep for approximately half a slot
        await sleep(MS_PER_SLOT / 2);
      }

      throw new Error(
        `Unable to obtain a new blockhash after ${Date.now() - startTime}ms`,
      );
    } finally {
      this._pollingBlockhash = false;
    }
  }

  /**
   * Simulate a transaction
   */
  async simulateTransaction(
    transactionOrMessage: Transaction | Message,
    signers?: Array<Signer>,
    includeAccounts?: boolean | Array<PublicKey>,
  ): Promise<RpcResponseAndContext<SimulatedTransactionResponse>> {
    let transaction;
    if (transactionOrMessage instanceof Transaction) {
      let originalTx: Transaction = transactionOrMessage;
      transaction = new Transaction();
      transaction.feePayer = originalTx.feePayer;
      transaction.instructions = transactionOrMessage.instructions;
      transaction.nonceInfo = originalTx.nonceInfo;
      transaction.signatures = originalTx.signatures;
    } else {
      transaction = Transaction.populate(transactionOrMessage);
      // HACK: this function relies on mutating the populated transaction
      transaction._message = transaction._json = undefined;
    }

    if (transaction.nonceInfo && signers) {
      transaction.sign(...signers);
    } else {
      let disableCache = this._disableBlockhashCaching;
      for (;;) {
        const latestBlockhash = await this._blockhashWithExpiryBlockHeight(
          disableCache,
        );
        transaction.lastValidBlockHeight = latestBlockhash.lastValidBlockHeight;
        transaction.recentBlockhash = latestBlockhash.blockhash;

        if (!signers) break;

        transaction.sign(...signers);
        if (!transaction.signature) {
          throw new Error('!signature'); // should never happen
        }

        const signature = transaction.signature.toString('base64');
        if (
          !this._blockhashInfo.simulatedSignatures.includes(signature) &&
          !this._blockhashInfo.transactionSignatures.includes(signature)
        ) {
          // The signature of this transaction has not been seen before with the
          // current recentBlockhash, all done. Let's break
          this._blockhashInfo.simulatedSignatures.push(signature);
          break;
        } else {
          // This transaction would be treated as duplicate (its derived signature
          // matched to one of already recorded signatures).
          // So, we must fetch a new blockhash for a different signature by disabling
          // our cache not to wait for the cache expiration (BLOCKHASH_CACHE_TIMEOUT_MS).
          disableCache = true;
        }
      }
    }

    const message = transaction._compile();
    const signData = message.serialize();
    const wireTransaction = transaction._serialize(signData);
    const encodedTransaction = wireTransaction.toString('base64');
    const config: any = {
      encoding: 'base64',
      commitment: this.commitment,
    };

    if (includeAccounts) {
      const addresses = (
        Array.isArray(includeAccounts)
          ? includeAccounts
          : message.nonProgramIds()
      ).map(key => key.toBase58());

      config['accounts'] = {
        encoding: 'base64',
        addresses,
      };
    }

    if (signers) {
      config.sigVerify = true;
    }

    const args = [encodedTransaction, config];
    const unsafeRes = await this._rpcRequest('simulateTransaction', args);
    const res = create(unsafeRes, SimulatedTransactionResponseStruct);
    if ('error' in res) {
      let logs;
      if ('data' in res.error) {
        logs = res.error.data.logs;
        if (logs && Array.isArray(logs)) {
          const traceIndent = '\n    ';
          const logTrace = traceIndent + logs.join(traceIndent);
          console.error(res.error.message, logTrace);
        }
      }
      throw new SendTransactionError(
        'failed to simulate transaction: ' + res.error.message,
        logs,
      );
    }
    return res.result;
  }

  /**
   * Sign and send a transaction
   */
  async sendTransaction(
    transaction: Transaction,
    signers: Array<Signer>,
    options?: SendOptions,
  ): Promise<TransactionSignature> {
    if (transaction.nonceInfo) {
      transaction.sign(...signers);
    } else {
      let disableCache = this._disableBlockhashCaching;
      for (;;) {
        const latestBlockhash = await this._blockhashWithExpiryBlockHeight(
          disableCache,
        );
        transaction.lastValidBlockHeight = latestBlockhash.lastValidBlockHeight;
        transaction.recentBlockhash = latestBlockhash.blockhash;
        transaction.sign(...signers);
        if (!transaction.signature) {
          throw new Error('!signature'); // should never happen
        }

        const signature = transaction.signature.toString('base64');
        if (!this._blockhashInfo.transactionSignatures.includes(signature)) {
          // The signature of this transaction has not been seen before with the
          // current recentBlockhash, all done. Let's break
          this._blockhashInfo.transactionSignatures.push(signature);
          break;
        } else {
          // This transaction would be treated as duplicate (its derived signature
          // matched to one of already recorded signatures).
          // So, we must fetch a new blockhash for a different signature by disabling
          // our cache not to wait for the cache expiration (BLOCKHASH_CACHE_TIMEOUT_MS).
          disableCache = true;
        }
      }
    }

    const wireTransaction = transaction.serialize();
    return await this.sendRawTransaction(wireTransaction, options);
  }

  /**
   * Send a transaction that has already been signed and serialized into the
   * wire format
   */
  async sendRawTransaction(
    rawTransaction: Buffer | Uint8Array | Array<number>,
    options?: SendOptions,
  ): Promise<TransactionSignature> {
    const encodedTransaction = toBuffer(rawTransaction).toString('base64');
    const result = await this.sendEncodedTransaction(
      encodedTransaction,
      options,
    );
    return result;
  }

  /**
   * Send a transaction that has already been signed, serialized into the
   * wire format, and encoded as a base64 string
   */
  async sendEncodedTransaction(
    encodedTransaction: string,
    options?: SendOptions,
  ): Promise<TransactionSignature> {
    const config: any = {encoding: 'base64'};
    const skipPreflight = options && options.skipPreflight;
    const preflightCommitment =
      (options && options.preflightCommitment) || this.commitment;

    if (options && options.maxRetries) {
      config.maxRetries = options.maxRetries;
    }
    if (skipPreflight) {
      config.skipPreflight = skipPreflight;
    }
    if (preflightCommitment) {
      config.preflightCommitment = preflightCommitment;
    }

    const args = [encodedTransaction, config];
    const unsafeRes = await this._rpcRequest('sendTransaction', args);
    const res = create(unsafeRes, SendTransactionRpcResult);
    if ('error' in res) {
      let logs;
      if ('data' in res.error) {
        logs = res.error.data.logs;
      }
      throw new SendTransactionError(
        'failed to send transaction: ' + res.error.message,
        logs,
      );
    }
    return res.result;
  }

  /**
   * @internal
   */
  _wsOnOpen() {
    this._rpcWebSocketConnected = true;
    this._rpcWebSocketHeartbeat = setInterval(() => {
      // Ping server every 5s to prevent idle timeouts
      this._rpcWebSocket.notify('ping').catch(() => {});
    }, 5000);
    this._updateSubscriptions();
  }

  /**
   * @internal
   */
  _wsOnError(err: Error) {
    this._rpcWebSocketConnected = false;
    console.error('ws error:', err.message);
  }

  /**
   * @internal
   */
  _wsOnClose(code: number) {
    this._rpcWebSocketConnected = false;
    this._rpcWebSocketGeneration++;
    if (this._rpcWebSocketHeartbeat) {
      clearInterval(this._rpcWebSocketHeartbeat);
      this._rpcWebSocketHeartbeat = null;
    }

    if (code === 1000) {
      // explicit close, check if any subscriptions have been made since close
      this._updateSubscriptions();
      return;
    }

    // implicit close, prepare subscriptions for auto-reconnect
    this._subscriptionCallbacksByServerSubscriptionId = {};
    Object.entries(
      this._subscriptionsByHash as Record<SubscriptionConfigHash, Subscription>,
    ).forEach(([hash, subscription]) => {
      this._subscriptionsByHash[hash] = {
        ...subscription,
        state: 'pending',
      };
    });
  }

  /**
   * @internal
   */
  async _updateSubscriptions() {
    if (Object.keys(this._subscriptionsByHash).length === 0) {
      if (this._rpcWebSocketConnected) {
        this._rpcWebSocketConnected = false;
        this._rpcWebSocketIdleTimeout = setTimeout(() => {
          this._rpcWebSocketIdleTimeout = null;
          try {
            this._rpcWebSocket.close();
          } catch (err) {
            // swallow error if socket has already been closed.
            if (err instanceof Error) {
              console.log(
                `Error when closing socket connection: ${err.message}`,
              );
            }
          }
        }, 500);
      }
      return;
    }

    if (this._rpcWebSocketIdleTimeout !== null) {
      clearTimeout(this._rpcWebSocketIdleTimeout);
      this._rpcWebSocketIdleTimeout = null;
      this._rpcWebSocketConnected = true;
    }

    if (!this._rpcWebSocketConnected) {
      this._rpcWebSocket.connect();
      return;
    }

    const activeWebSocketGeneration = this._rpcWebSocketGeneration;
    const isCurrentConnectionStillActive = () => {
      return activeWebSocketGeneration === this._rpcWebSocketGeneration;
    };

    await Promise.all(
      // Don't be tempted to change this to `Object.entries`. We call
      // `_updateSubscriptions` recursively when processing the state,
      // so it's important that we look up the *current* version of
      // each subscription, every time we process a hash.
      Object.keys(this._subscriptionsByHash).map(async hash => {
        const subscription = this._subscriptionsByHash[hash];
        if (subscription === undefined) {
          // This entry has since been deleted. Skip.
          return;
        }
        switch (subscription.state) {
          case 'pending':
          case 'unsubscribed':
            if (subscription.callbacks.size === 0) {
              /**
               * You can end up here when:
               *
               * - a subscription has recently unsubscribed
               *   without having new callbacks added to it
               *   while the unsubscribe was in flight, or
               * - when a pending subscription has its
               *   listeners removed before a request was
               *   sent to the server.
               *
               * Being that nobody is interested in this
               * subscription any longer, delete it.
               */
              delete this._subscriptionsByHash[hash];
              if (subscription.state === 'unsubscribed') {
                delete this._subscriptionCallbacksByServerSubscriptionId[
                  subscription.serverSubscriptionId
                ];
              }
              await this._updateSubscriptions();
              return;
            }
            await (async () => {
              const {args, method} = subscription;
              try {
                this._subscriptionsByHash[hash] = {
                  ...subscription,
                  state: 'subscribing',
                };
                const serverSubscriptionId: ServerSubscriptionId =
                  (await this._rpcWebSocket.call(method, args)) as number;
                this._subscriptionsByHash[hash] = {
                  ...subscription,
                  serverSubscriptionId,
                  state: 'subscribed',
                };
                this._subscriptionCallbacksByServerSubscriptionId[
                  serverSubscriptionId
                ] = subscription.callbacks;
                await this._updateSubscriptions();
              } catch (e) {
                if (e instanceof Error) {
                  console.error(
                    `${method} error for argument`,
                    args,
                    e.message,
                  );
                }
                if (!isCurrentConnectionStillActive()) {
                  return;
                }
                // TODO: Maybe add an 'errored' state or a retry limit?
                this._subscriptionsByHash[hash] = {
                  ...subscription,
                  state: 'pending',
                };
                await this._updateSubscriptions();
              }
            })();
            break;
          case 'subscribed':
            if (subscription.callbacks.size === 0) {
              // By the time we successfully set up a subscription
              // with the server, the client stopped caring about it.
              // Tear it down now.
              await (async () => {
                const {serverSubscriptionId, unsubscribeMethod} = subscription;
                if (
                  this._subscriptionsAutoDisposedByRpc.has(serverSubscriptionId)
                ) {
                  /**
                   * Special case.
                   * If we're dealing with a subscription that has been auto-
                   * disposed by the RPC, then we can skip the RPC call to
                   * tear down the subscription here.
                   *
                   * NOTE: There is a proposal to eliminate this special case, here:
                   * https://github.com/solana-labs/solana/issues/18892
                   */
                  this._subscriptionsAutoDisposedByRpc.delete(
                    serverSubscriptionId,
                  );
                } else {
                  this._subscriptionsByHash[hash] = {
                    ...subscription,
                    state: 'unsubscribing',
                  };
                  try {
                    await this._rpcWebSocket.call(unsubscribeMethod, [
                      serverSubscriptionId,
                    ]);
                  } catch (e) {
                    if (e instanceof Error) {
                      console.error(`${unsubscribeMethod} error:`, e.message);
                    }
                    if (!isCurrentConnectionStillActive()) {
                      return;
                    }
                    // TODO: Maybe add an 'errored' state or a retry limit?
                    this._subscriptionsByHash[hash] = {
                      ...subscription,
                      state: 'subscribed',
                    };
                    await this._updateSubscriptions();
                    return;
                  }
                }
                this._subscriptionsByHash[hash] = {
                  ...subscription,
                  state: 'unsubscribed',
                };
                await this._updateSubscriptions();
              })();
            }
            break;
          case 'subscribing':
          case 'unsubscribing':
            break;
        }
      }),
    );
  }

  /**
   * @internal
   */
  private _handleServerNotification<
    TCallback extends SubscriptionConfig['callback'],
  >(
    serverSubscriptionId: ServerSubscriptionId,
    callbackArgs: Parameters<TCallback>,
  ): void {
    const callbacks =
      this._subscriptionCallbacksByServerSubscriptionId[serverSubscriptionId];
    if (callbacks === undefined) {
      return;
    }
    callbacks.forEach(cb => {
      try {
        cb(
          // I failed to find a way to convince TypeScript that `cb` is of type
          // `TCallback` which is certainly compatible with `Parameters<TCallback>`.
          // See https://github.com/microsoft/TypeScript/issues/47615
          // @ts-ignore
          ...callbackArgs,
        );
      } catch (e) {
        console.error(e);
      }
    });
  }

  /**
   * @internal
   */
  _wsOnAccountNotification(notification: object) {
    const {result, subscription} = create(
      notification,
      AccountNotificationResult,
    );
    this._handleServerNotification<AccountChangeCallback>(subscription, [
      result.value,
      result.context,
    ]);
  }

  /**
   * @internal
   */
  private _makeSubscription(
    subscriptionConfig: SubscriptionConfig,
    /**
     * When preparing `args` for a call to `_makeSubscription`, be sure
     * to carefully apply a default `commitment` property, if necessary.
     *
     * - If the user supplied a `commitment` use that.
     * - Otherwise, if the `Connection::commitment` is set, use that.
     * - Otherwise, set it to the RPC server default: `finalized`.
     *
     * This is extremely important to ensure that these two fundamentally
     * identical subscriptions produce the same identifying hash:
     *
     * - A subscription made without specifying a commitment.
     * - A subscription made where the commitment specified is the same
     *   as the default applied to the subscription above.
     *
     * Example; these two subscriptions must produce the same hash:
     *
     * - An `accountSubscribe` subscription for `'PUBKEY'`
     * - An `accountSubscribe` subscription for `'PUBKEY'` with commitment
     *   `'finalized'`.
     *
     * See the 'making a subscription with defaulted params omitted' test
     * in `connection-subscriptions.ts` for more.
     */
    args: IWSRequestParams,
  ): ClientSubscriptionId {
    const clientSubscriptionId = this._nextClientSubscriptionId++;
    const hash = fastStableStringify(
      [subscriptionConfig.method, args],
      true /* isArrayProp */,
    );
    const existingSubscription = this._subscriptionsByHash[hash];
    if (existingSubscription === undefined) {
      this._subscriptionsByHash[hash] = {
        ...subscriptionConfig,
        args,
        callbacks: new Set([subscriptionConfig.callback]),
        state: 'pending',
      };
    } else {
      existingSubscription.callbacks.add(subscriptionConfig.callback);
    }
    this._subscriptionDisposeFunctionsByClientSubscriptionId[
      clientSubscriptionId
    ] = async () => {
      delete this._subscriptionDisposeFunctionsByClientSubscriptionId[
        clientSubscriptionId
      ];
      const subscription = this._subscriptionsByHash[hash];
      assert(
        subscription !== undefined,
        `Could not find a \`Subscription\` when tearing down client subscription #${clientSubscriptionId}`,
      );
      subscription.callbacks.delete(subscriptionConfig.callback);
      await this._updateSubscriptions();
    };
    this._updateSubscriptions();
    return clientSubscriptionId;
  }

  /**
   * Register a callback to be invoked whenever the specified account changes
   *
   * @param publicKey Public key of the account to monitor
   * @param callback Function to invoke whenever the account is changed
   * @param commitment Specify the commitment level account changes must reach before notification
   * @return subscription id
   */
  onAccountChange(
    publicKey: PublicKey,
    callback: AccountChangeCallback,
    commitment?: Commitment,
  ): ClientSubscriptionId {
    const args = this._buildArgs(
      [publicKey.toBase58()],
      commitment || this._commitment || 'finalized', // Apply connection/server default.
      'base64',
    );
    return this._makeSubscription(
      {
        callback,
        method: 'accountSubscribe',
        unsubscribeMethod: 'accountUnsubscribe',
      },
      args,
    );
  }

  /**
   * Deregister an account notification callback
   *
   * @param id client subscription id to deregister
   */
  async removeAccountChangeListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(
      clientSubscriptionId,
      'account change',
    );
  }

  /**
   * @internal
   */
  _wsOnProgramAccountNotification(notification: Object) {
    const {result, subscription} = create(
      notification,
      ProgramAccountNotificationResult,
    );
    this._handleServerNotification<ProgramAccountChangeCallback>(subscription, [
      {
        accountId: result.value.pubkey,
        accountInfo: result.value.account,
      },
      result.context,
    ]);
  }

  /**
   * Register a callback to be invoked whenever accounts owned by the
   * specified program change
   *
   * @param programId Public key of the program to monitor
   * @param callback Function to invoke whenever the account is changed
   * @param commitment Specify the commitment level account changes must reach before notification
   * @param filters The program account filters to pass into the RPC method
   * @return subscription id
   */
  onProgramAccountChange(
    programId: PublicKey,
    callback: ProgramAccountChangeCallback,
    commitment?: Commitment,
    filters?: GetProgramAccountsFilter[],
  ): ClientSubscriptionId {
    const args = this._buildArgs(
      [programId.toBase58()],
      commitment || this._commitment || 'finalized', // Apply connection/server default.
      'base64' /* encoding */,
      filters ? {filters: filters} : undefined /* extra */,
    );
    return this._makeSubscription(
      {
        callback,
        method: 'programSubscribe',
        unsubscribeMethod: 'programUnsubscribe',
      },
      args,
    );
  }

  /**
   * Deregister an account notification callback
   *
   * @param id client subscription id to deregister
   */
  async removeProgramAccountChangeListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(
      clientSubscriptionId,
      'program account change',
    );
  }

  /**
   * Registers a callback to be invoked whenever logs are emitted.
   */
  onLogs(
    filter: LogsFilter,
    callback: LogsCallback,
    commitment?: Commitment,
  ): ClientSubscriptionId {
    const args = this._buildArgs(
      [typeof filter === 'object' ? {mentions: [filter.toString()]} : filter],
      commitment || this._commitment || 'finalized', // Apply connection/server default.
    );
    return this._makeSubscription(
      {
        callback,
        method: 'logsSubscribe',
        unsubscribeMethod: 'logsUnsubscribe',
      },
      args,
    );
  }

  /**
   * Deregister a logs callback.
   *
   * @param id client subscription id to deregister.
   */
  async removeOnLogsListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(clientSubscriptionId, 'logs');
  }

  /**
   * @internal
   */
  _wsOnLogsNotification(notification: Object) {
    const {result, subscription} = create(notification, LogsNotificationResult);
    this._handleServerNotification<LogsCallback>(subscription, [
      result.value,
      result.context,
    ]);
  }

  /**
   * @internal
   */
  _wsOnSlotNotification(notification: Object) {
    const {result, subscription} = create(notification, SlotNotificationResult);
    this._handleServerNotification<SlotChangeCallback>(subscription, [result]);
  }

  /**
   * Register a callback to be invoked upon slot changes
   *
   * @param callback Function to invoke whenever the slot changes
   * @return subscription id
   */
  onSlotChange(callback: SlotChangeCallback): ClientSubscriptionId {
    return this._makeSubscription(
      {
        callback,
        method: 'slotSubscribe',
        unsubscribeMethod: 'slotUnsubscribe',
      },
      [] /* args */,
    );
  }

  /**
   * Deregister a slot notification callback
   *
   * @param id client subscription id to deregister
   */
  async removeSlotChangeListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(
      clientSubscriptionId,
      'slot change',
    );
  }

  /**
   * @internal
   */
  _wsOnSlotUpdatesNotification(notification: Object) {
    const {result, subscription} = create(
      notification,
      SlotUpdateNotificationResult,
    );
    this._handleServerNotification<SlotUpdateCallback>(subscription, [result]);
  }

  /**
   * Register a callback to be invoked upon slot updates. {@link SlotUpdate}'s
   * may be useful to track live progress of a cluster.
   *
   * @param callback Function to invoke whenever the slot updates
   * @return subscription id
   */
  onSlotUpdate(callback: SlotUpdateCallback): ClientSubscriptionId {
    return this._makeSubscription(
      {
        callback,
        method: 'slotsUpdatesSubscribe',
        unsubscribeMethod: 'slotsUpdatesUnsubscribe',
      },
      [] /* args */,
    );
  }

  /**
   * Deregister a slot update notification callback
   *
   * @param id client subscription id to deregister
   */
  async removeSlotUpdateListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(
      clientSubscriptionId,
      'slot update',
    );
  }

  /**
   * @internal
   */

  private async _unsubscribeClientSubscription(
    clientSubscriptionId: ClientSubscriptionId,
    subscriptionName: string,
  ) {
    const dispose =
      this._subscriptionDisposeFunctionsByClientSubscriptionId[
        clientSubscriptionId
      ];
    if (dispose) {
      await dispose();
    } else {
      console.warn(
        'Ignored unsubscribe request because an active subscription with id ' +
          `\`${clientSubscriptionId}\` for '${subscriptionName}' events ` +
          'could not be found.',
      );
    }
  }

  _buildArgs(
    args: Array<any>,
    override?: Commitment,
    encoding?: 'jsonParsed' | 'base64',
    extra?: any,
  ): Array<any> {
    const commitment = override || this._commitment;
    if (commitment || encoding || extra) {
      let options: any = {};
      if (encoding) {
        options.encoding = encoding;
      }
      if (commitment) {
        options.commitment = commitment;
      }
      if (extra) {
        options = Object.assign(options, extra);
      }
      args.push(options);
    }
    return args;
  }

  /**
   * @internal
   */
  _buildArgsAtLeastConfirmed(
    args: Array<any>,
    override?: Finality,
    encoding?: 'jsonParsed' | 'base64',
    extra?: any,
  ): Array<any> {
    const commitment = override || this._commitment;
    if (commitment && !['confirmed', 'finalized'].includes(commitment)) {
      throw new Error(
        'Using Connection with default commitment: `' +
          this._commitment +
          '`, but method requires at least `confirmed`',
      );
    }
    return this._buildArgs(args, override, encoding, extra);
  }

  /**
   * @internal
   */
  _wsOnSignatureNotification(notification: Object) {
    const {result, subscription} = create(
      notification,
      SignatureNotificationResult,
    );
    if (result.value !== 'receivedSignature') {
      /**
       * Special case.
       * After a signature is processed, RPCs automatically dispose of the
       * subscription on the server side. We need to track which of these
       * subscriptions have been disposed in such a way, so that we know
       * whether the client is dealing with a not-yet-processed signature
       * (in which case we must tear down the server subscription) or an
       * already-processed signature (in which case the client can simply
       * clear out the subscription locally without telling the server).
       *
       * NOTE: There is a proposal to eliminate this special case, here:
       * https://github.com/solana-labs/solana/issues/18892
       */
      this._subscriptionsAutoDisposedByRpc.add(subscription);
    }
    this._handleServerNotification<SignatureSubscriptionCallback>(
      subscription,
      result.value === 'receivedSignature'
        ? [{type: 'received'}, result.context]
        : [{type: 'status', result: result.value}, result.context],
    );
  }

  /**
   * Register a callback to be invoked upon signature updates
   *
   * @param signature Transaction signature string in base 58
   * @param callback Function to invoke on signature notifications
   * @param commitment Specify the commitment level signature must reach before notification
   * @return subscription id
   */
  onSignature(
    signature: TransactionSignature,
    callback: SignatureResultCallback,
    commitment?: Commitment,
  ): ClientSubscriptionId {
    const args = this._buildArgs(
      [signature],
      commitment || this._commitment || 'finalized', // Apply connection/server default.
    );
    const clientSubscriptionId = this._makeSubscription(
      {
        callback: (notification, context) => {
          if (notification.type === 'status') {
            callback(notification.result, context);
            // Signatures subscriptions are auto-removed by the RPC service
            // so no need to explicitly send an unsubscribe message.
            try {
              this.removeSignatureListener(clientSubscriptionId);
              // eslint-disable-next-line no-empty
            } catch (_err) {
              // Already removed.
            }
          }
        },
        method: 'signatureSubscribe',
        unsubscribeMethod: 'signatureUnsubscribe',
      },
      args,
    );
    return clientSubscriptionId;
  }

  /**
   * Register a callback to be invoked when a transaction is
   * received and/or processed.
   *
   * @param signature Transaction signature string in base 58
   * @param callback Function to invoke on signature notifications
   * @param options Enable received notifications and set the commitment
   *   level that signature must reach before notification
   * @return subscription id
   */
  onSignatureWithOptions(
    signature: TransactionSignature,
    callback: SignatureSubscriptionCallback,
    options?: SignatureSubscriptionOptions,
  ): ClientSubscriptionId {
    const {commitment, ...extra} = {
      ...options,
      commitment:
        (options && options.commitment) || this._commitment || 'finalized', // Apply connection/server default.
    };
    const args = this._buildArgs(
      [signature],
      commitment,
      undefined /* encoding */,
      extra,
    );
    const clientSubscriptionId = this._makeSubscription(
      {
        callback: (notification, context) => {
          callback(notification, context);
          // Signatures subscriptions are auto-removed by the RPC service
          // so no need to explicitly send an unsubscribe message.
          try {
            this.removeSignatureListener(clientSubscriptionId);
            // eslint-disable-next-line no-empty
          } catch (_err) {
            // Already removed.
          }
        },
        method: 'signatureSubscribe',
        unsubscribeMethod: 'signatureUnsubscribe',
      },
      args,
    );
    return clientSubscriptionId;
  }

  /**
   * Deregister a signature notification callback
   *
   * @param id client subscription id to deregister
   */
  async removeSignatureListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(
      clientSubscriptionId,
      'signature result',
    );
  }

  /**
   * @internal
   */
  _wsOnRootNotification(notification: Object) {
    const {result, subscription} = create(notification, RootNotificationResult);
    this._handleServerNotification<RootChangeCallback>(subscription, [result]);
  }

  /**
   * Register a callback to be invoked upon root changes
   *
   * @param callback Function to invoke whenever the root changes
   * @return subscription id
   */
  onRootChange(callback: RootChangeCallback): ClientSubscriptionId {
    return this._makeSubscription(
      {
        callback,
        method: 'rootSubscribe',
        unsubscribeMethod: 'rootUnsubscribe',
      },
      [] /* args */,
    );
  }

  /**
   * Deregister a root notification callback
   *
   * @param id client subscription id to deregister
   */
  async removeRootChangeListener(
    clientSubscriptionId: ClientSubscriptionId,
  ): Promise<void> {
    await this._unsubscribeClientSubscription(
      clientSubscriptionId,
      'root change',
    );
  }
}
