#!/bin/bash

echo "Building benchmark..."
cargo build --release

echo "Running connection benchmarks..."
echo "database,iterations,p50,p90,p95,p99,p999,p9999,p99999" > results.csv

# Test each database with different table counts
for db in database_10.db database_1k.db database_5k.db database_10k.db
do
  echo "Testing $db..."
  ./target/release/limbo-connection-benchmark $db --iterations 1000 | tail -1 >> results.csv
done

echo "Results written to results.csv"
cat results.csv
