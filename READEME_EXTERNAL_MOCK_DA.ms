# Overview

This readme explains how to run the rollup using an external mock DA.
Before proceeding, make sure you’ve read the main README.md, as we’ll skip explanations already covered there.

### 1. Start the exernal mock-da:
```bash,test-ci,bashtestmd:long-running,bashtestmd:wait-until=rest_address
$ cargo run --bin mock-da-server --no-default-features --features="mock_da_external,mock_zkvm"
```

### 2. Start the rollup node:

```bash,test-ci,bashtestmd:long-running,bashtestmd:wait-until=rest_address
$ cargo run  --no-default-features --features="mock_da_external,mock_zkvm"
```

### Explore the REST API endpoints via Swagger UI

The rollup includes several built-in modules: Bank (for token management), Paymaster, Hyperlane, and more. You can query any state item in these modules:

```bash
open http://127.0.0.1:12346/swagger-ui/#/ 
```

### Example: Query the `ValueSetter` Module's state value

For now, you should just see null returned for the value state item, as the item hasn't been initialized:

```bash,test-ci,bashtestmd:compare-output
$ curl http://127.0.0.1:12346/modules/value-setter/state/value
{"value":null}
```

## Programmatic Interaction with TypeScript

### Set up the TypeScript client:

```bash,test-ci,bashtestmd:exit-code=0
$ cd examples/starter-js && npm install
```

### The TypeScript script demonstrates the complete transaction flow:

```js
// 1. Initialize rollup client
// defaults to http://localhost:12346, or pass url: "custom-endpoint"
const rollup = await createStandardRollup();

// 2. Initialize signer
const privKey = "0d87c12ea7c12024b3f70a26d735874608f17c8bce2b48e6fe87389310191264";
let signer = new Secp256k1Signer(privKey, chainHash);

// 3. Create a transaction (call message)
let callMessage: RuntimeCall = {
  bank: {
    create_token: {
      admins: [],
      token_decimals: 8,
      supply_cap: 100000000000,
      token_name: "Example Token",
      initial_balance: 1000000000,
      mint_to_address: signerAddress, // derived from privKey above (can be any valid address)
    },
  },
};

// 4. Send transaction
let tx_response = await rollup.call(callMessage, { signer });
```

### Run the script

You should see a transaction soft-confirmation with events:

```bash,test-ci,bashtestmd:exit-code=0
$ npm run start
Initializing rollup client...
Rollup client initialized.
Initializing signer...
Signer initialized.
Signer address: 0x9b08ce57a93751ae790698a2c9ebc76a78f23e25
Sending create token transaction...
Tx sent successfully. Response:
{
  id: '0x633b06f81b2884f8f40a3f06535cdbedb859c37d328c24fd4518377c78dac60e',
  events: [
    {
      type: 'event',
      number: 0,
      key: 'Bank/TokenCreated',
      value: {
        token_created: {
          token_name: 'Example Token',
          coins: {
            amount: '1000000000',
            token_id: 'token_10jrdwqkd0d4zf775np8x3tx29rk7j5m0nz9wj8t7czshylwhnsyqpgqtr9'
          },
          mint_to_address: { user: '0x9b08ce57a93751ae790698a2c9ebc76a78f23e25' },
          minter: { user: '0x9b08ce57a93751ae790698a2c9ebc76a78f23e25' },
          supply_cap: '100000000000',
          admins: []
        }
      },
      module: { type: 'moduleRef', name: 'Bank' },
      tx_hash: '0x633b06f81b2884f8f40a3f06535cdbedb859c37d328c24fd4518377c78dac60e'
    }
  ],
  receipt: { result: 'successful', data: { gas_used: [ 21119, 21119 ] } },
  tx_number: 0,
  status: 'submitted'
}
```
