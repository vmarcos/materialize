# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET max_clusters = 20

$ kafka-create-topic topic=test partitions=1
$ kafka-ingest topic=test format=bytes
jack,jill
goofus,gallant

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CLUSTER src_cluster SIZE '${arg.default-storage-size}';
> CREATE SOURCE src (a, b)
  IN CLUSTER src_cluster
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-test-${testdrive.seed}')
  FORMAT CSV WITH 2 COLUMNS
  INCLUDE OFFSET

> CREATE CLUSTER src_materialized_cluster SIZE '${arg.default-storage-size}';
> CREATE SOURCE src_materialized (a, b)
  IN CLUSTER src_materialized_cluster
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-test-${testdrive.seed}')
  FORMAT CSV WITH 2 COLUMNS
  INCLUDE OFFSET

> CREATE VIEW v1 AS
  SELECT a || b AS c FROM src

> CREATE VIEW v2 AS
  SELECT a || b AS c FROM src_materialized

> CREATE MATERIALIZED VIEW v3 AS
  SELECT a || b AS c FROM src

# We should refuse to create a sink with invalid WITH options

! CREATE SINK invalid_with_option
  IN CLUSTER ${arg.single-replica-cluster}
  FROM src
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  WITH (badoption=true)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
contains:Expected one of SIZE or SNAPSHOT

> SHOW SINKS
name               type   size  cluster
---------------------------------------

# # We should refuse to create a sink with an invalid schema registry URL.
#
# # Invalid in that the address is not well formed
# ! CREATE SINK bad_schema_registry
#   IN CLUSTER ${arg.single-replica-cluster}
#   FROM v3
#   INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
#   FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.kafka-addr}'
# contains:cannot construct a CCSR client with a cannot-be-a-base URL
#
# # Invalid in that the address points to an invalid host
# ! CREATE SINK bad_schema_registry
#   IN CLUSTER ${arg.single-replica-cluster}
#   FROM v3
#   INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
#   FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY 'http://no-such-host'
# contains:unable to publish value schema to registry in kafka sink
#
# # Invalid in that the address is not for a schema registry
# ! CREATE SINK bad_schema_registry
#   IN CLUSTER ${arg.single-replica-cluster}
#   FROM v3
#   INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
#   FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY 'http://materialized:6875'
# contains:unable to publish value schema to registry in kafka sink
#
# ! CREATE SINK bad_view
#   IN CLUSTER ${arg.single-replica-cluster}
#   FROM v1
#   INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
#   FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
# contains:v1 is a view, which cannot be exported as a sink
#
# # ...Even if that view is based on a materialized source
# ! CREATE SINK bad_view2
#   IN CLUSTER ${arg.single-replica-cluster}
#   FROM v2
#   INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
#   WITH (retention_ms=1000000)
#   FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
# contains:v2 is a view, which cannot be exported as a sink
#
# > SHOW SINKS
# name               type   size  cluster
# ---------------------------------------

# N.B. it is important to test sinks that depend on sources directly vs. sinks
# that depend on views, as the code paths are different.

> CREATE CLUSTER snk1_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk1
  IN CLUSTER snk1_cluster
  FROM src
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> CREATE CLUSTER snk2_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk2
  IN CLUSTER snk2_cluster
  FROM src_materialized
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk2-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> CREATE CLUSTER snk3_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk3
  IN CLUSTER snk3_cluster
  FROM v3
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk3-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> SHOW SINKS
name               type   size  cluster
---------------------------------------
snk1             kafka  ${arg.default-storage-size}  snk1_cluster
snk2             kafka  ${arg.default-storage-size}  snk2_cluster
snk3             kafka  ${arg.default-storage-size}  snk3_cluster

$ kafka-verify-data format=avro sink=materialize.public.snk1 sort-messages=true
{"before": null, "after": {"row":{"a": "goofus", "b": "gallant", "offset": [0, 0, 0, 0, 0, 0, 0, 1]}}}
{"before": null, "after": {"row":{"a": "jack", "b": "jill", "offset": [0, 0, 0, 0, 0, 0, 0, 0]}}}

$ kafka-verify-data format=avro sink=materialize.public.snk2 sort-messages=true
{"before": null, "after": {"row":{"a": "goofus", "b": "gallant", "offset": [0, 0, 0, 0, 0, 0, 0, 1]}}}
{"before": null, "after": {"row":{"a": "jack", "b": "jill", "offset": [0, 0, 0, 0, 0, 0, 0, 0]}}}

$ kafka-verify-data format=avro sink=materialize.public.snk3 sort-messages=true
{"before": null, "after": {"row":{"c": "goofusgallant"}}}
{"before": null, "after": {"row":{"c": "jackjill"}}}

# Test Avro serialization of unsigned values.
> CREATE MATERIALIZED VIEW unsigned (a, b, c, d, e, f) AS
  VALUES ('1'::uint2, '2'::uint2, '3'::uint4, '4'::uint4, '5'::uint8, '6'::uint8)
> CREATE CLUSTER snk_unsigned_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk_unsigned
  IN CLUSTER snk_unsigned_cluster
  FROM unsigned
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'snk2')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
$ kafka-verify-data format=avro sink=materialize.public.snk_unsigned sort-messages=true
{"before": null, "after": {"row":{"a": [0, 1], "b": [0, 2], "c": [0, 0, 0, 3], "d": [0, 0, 0, 4], "e": [0, 0, 0, 0, 0, 0, 0, 5], "f": [0, 0, 0, 0, 0, 0, 0, 6]}}}

# Test the case where we have non +/- 1 multiplicities

> CREATE MATERIALIZED VIEW v4 AS
  SELECT true AS c FROM src

> CREATE CLUSTER snk4_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk4
  IN CLUSTER snk4_cluster
  FROM v4
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk4-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.snk4
{"before": null, "after": {"row":{"c": true}}}
{"before": null, "after": {"row":{"c": true}}}

# Test WITH (SNAPSHOT).
#
# N.B. It's important that we've verified above that a sink exporting
# src_materialized has processed the row. This means the data has a definite
# timestamp.  Without that, WITH (SNAPSHOT = false) could correct either include or
# exclude the old rows.

> CREATE CLUSTER snk5_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk5
  IN CLUSTER snk5_cluster
  FROM src_materialized
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk5-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
  WITH (SNAPSHOT = false)

> CREATE CLUSTER snk6_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk6
  IN CLUSTER snk6_cluster
  FROM src_materialized
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk6-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
  WITH (SNAPSHOT = true)

$ kafka-ingest topic=test format=bytes
extra,row

$ kafka-verify-data format=avro sink=materialize.public.snk5
{"before": null, "after": {"row":{"a": "extra", "b": "row", "offset": [0, 0, 0, 0, 0, 0, 0, 2]}}}

$ kafka-verify-data format=avro sink=materialize.public.snk6 sort-messages=true
{"before": null, "after": {"row":{"a": "extra", "b": "row", "offset": [0, 0, 0, 0, 0, 0, 0, 2]}}}
{"before": null, "after": {"row":{"a": "goofus", "b": "gallant", "offset": [0, 0, 0, 0, 0, 0, 0, 1]}}}
{"before": null, "after": {"row":{"a": "jack", "b": "jill", "offset": [0, 0, 0, 0, 0, 0, 0, 0]}}}

# Test that we are correctly handling SNAPSHOT on views with empty upper
# frontier
> CREATE MATERIALIZED VIEW foo AS VALUES (1), (2), (3);

> CREATE CLUSTER snk7_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk7
  IN CLUSTER snk7_cluster
  FROM foo
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk7-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
  WITH (SNAPSHOT = false)

> CREATE CLUSTER snk8_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk8
  IN CLUSTER snk8_cluster
  FROM foo
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk8-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
  WITH (SNAPSHOT)

$ kafka-verify-data format=avro sink=materialize.public.snk8 sort-messages=true
{"before": null, "after": {"row":{"column1": 1}}}
{"before": null, "after": {"row":{"column1": 2}}}
{"before": null, "after": {"row":{"column1": 3}}}

# test already existing topic with non-default partition count
$ kafka-create-topic topic=snk9 partitions=4

> CREATE CLUSTER snk9_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK snk9
  IN CLUSTER snk9_cluster
  FROM foo
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk14-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> SET cluster TO ${arg.single-replica-cluster}

# create sink without specifying CLUSTER
> CREATE SINK default_cluster_sink
  FROM src
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> SET cluster TO default

# create sink with vestigial size specified
! CREATE SINK sink_with_size FROM src
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
  WITH (SIZE = '2')
contains:specifying sink SIZE deprecated; use IN CLUSTER

# create sink with SNAPSHOT set
> CREATE CLUSTER sink_with_options_cluster SIZE '${arg.default-storage-size}';
> CREATE SINK sink_with_options
  IN CLUSTER sink_with_options_cluster
  FROM src
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
  WITH (SNAPSHOT = false)

> CREATE CLUSTER c SIZE '4'

> CREATE SINK cluster_c_sink
  IN CLUSTER c
  FROM src
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

# All sinks are unlinked
> SELECT bool_and(size IS NULL) FROM mz_sinks;
true

# Check that SHOW SINKS shows the size correctly
> SHOW SINKS
name               type   size  cluster
---------------------------------------
cluster_c_sink        kafka  4                            c
default_cluster_sink  kafka  ${arg.default-replica-size}  ${arg.single-replica-cluster}
sink_with_options     kafka  ${arg.default-storage-size}  sink_with_options_cluster
snk1                  kafka  ${arg.default-storage-size}  snk1_cluster
snk2                  kafka  ${arg.default-storage-size}  snk2_cluster
snk3                  kafka  ${arg.default-storage-size}  snk3_cluster
snk4                  kafka  ${arg.default-storage-size}  snk4_cluster
snk5                  kafka  ${arg.default-storage-size}  snk5_cluster
snk6                  kafka  ${arg.default-storage-size}  snk6_cluster
snk7                  kafka  ${arg.default-storage-size}  snk7_cluster
snk8                  kafka  ${arg.default-storage-size}  snk8_cluster
snk9                  kafka  ${arg.default-storage-size}  snk9_cluster
snk_unsigned          kafka  ${arg.default-storage-size}  snk_unsigned_cluster
