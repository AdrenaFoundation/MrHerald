# MrHerald

OpenSource GRPC rust client (Keeper) monitoring Liquidation and interfacing with Discord

See MrSablier for the automated orders counterpart.
See MrSablierStaking for the staking related counterpart.

## Build

`$> cargo build`
`$> cargo build --release`

## Run

`$> RUST_LOG=debug ./target/debug/mrherald --endpoint https://adrena-solanam-6f0c.mainnet.rpcpool.com/ --x-token <> --commitment processed`
`$> RUST_LOG=info ./target/debug/mrherald --endpoint https://adrena-solanam-6f0c.mainnet.rpcpool.com/ --x-token <> --commitment processed`

## Run as a service using (Daemon)[https://www.libslack.org/daemon/manual/daemon.1.html]

`daemon --name=mrherald --output=/home/ubuntu/MrHerald/logfile.log -- /home/ubuntu/MrHerald/target/release/mrherald --payer-keypair /home/ubuntu/MrHerald/mr_herald.json --endpoint https://adrena-solanam-6f0c.mainnet.rpcpool.com/<> --x-token <> --commitment processed`

### Monitor Daemon logs

`tail -f -n 100 ~/MrHerald/logfile.log | tspin`

### Stop Daemon

`daemon --name=mrherald --stop`