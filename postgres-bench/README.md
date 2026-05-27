# postgres-bench

A small Rust benchmark that repeatedly writes per-thread `bytea` values to a
Postgres table.

The benchmark creates this table when needed, clears it before each run, inserts
one key per worker, then each worker updates its own key for the configured
duration:

```sql
CREATE TABLE IF NOT EXISTS kv (
  "key" text PRIMARY KEY,
  value bytea NOT NULL
);
```

Example:

```sh
cargo run --release -- \
  --dsn postgresql://postgres:postgres@localhost:5432/postgres \
  --threads 20 \
  --value-size 4096 \
  --duration-secs 60
```

The summary includes total writes, throughput, MiB/sec, and per-update latency
percentiles for successful `UPDATE` calls.

Aurora PostgreSQL IAM auth example:

```sh
export RDSHOST='database-1.cluster-cxuusi0qk3kc.eu-north-1.rds.amazonaws.com'
export PGUSER='postgres'
export PGDATABASE='postgres'
export AWS_REGION='eu-north-1'

export PGPASSWORD="$(aws rds generate-db-auth-token \
  --hostname "$RDSHOST" \
  --port 5432 \
  --region "$AWS_REGION" \
  --username "$PGUSER")"

cargo run --release -- \
  --dsn "host=$RDSHOST port=5432 dbname=$PGDATABASE user=$PGUSER password=$PGPASSWORD sslmode=require" \
  --threads 20 \
  --value-size 4096 \
  --duration-secs 60
```
