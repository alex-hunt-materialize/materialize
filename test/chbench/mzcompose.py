# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

import requests

from materialize.mzcompose.composition import (
    Composition,
    WorkflowArgumentParser,
)
from materialize.mzcompose.service import Service
from materialize.mzcompose.services.debezium import Debezium
from materialize.mzcompose.services.kafka import Kafka
from materialize.mzcompose.services.materialized import Materialized
from materialize.mzcompose.services.metabase import Metabase
from materialize.mzcompose.services.mysql import MySql
from materialize.mzcompose.services.mz import Mz
from materialize.mzcompose.services.schema_registry import SchemaRegistry
from materialize.mzcompose.services.zookeeper import Zookeeper

SERVICES = [
    Zookeeper(),
    Kafka(auto_create_topics=True),
    SchemaRegistry(),
    Debezium(),
    MySql(root_password="rootpw"),
    Mz(app_password=""),
    Materialized(),
    Metabase(),
    Service(
        name="chbench",
        config={
            "mzbuild": "chbenchmark",
            "init": True,
            "volumes": ["mydata:/gen"],
        },
    ),
]


def workflow_default(c: Composition) -> None:
    def process(name: str) -> None:
        if name == "default":
            return
        with c.test_case(name):
            c.workflow(name)

    c.test_parts(list(c.workflows.keys()), process)


def workflow_no_load(c: Composition, parser: WorkflowArgumentParser) -> None:
    """Run CH-benCHmark without any load on Materialize"""

    # Parse arguments.
    parser.add_argument(
        "--wait", action="store_true", help="wait for the load generator to exit"
    )
    args, unknown_args = parser.parse_known_args()

    # Start Materialize.
    c.up("materialized")

    # Start MySQL and Debezium.
    c.up("zookeeper", "kafka", "schema-registry", "mysql", "debezium")

    # Generate initial data.
    c.run(
        "chbench",
        "gen",
        "--config-file-path=/etc/chbenchmark/mz-default-mysql.cfg",
        "--warehouses=1",
    )

    # Start Debezium.
    response = requests.post(
        f"http://localhost:{c.default_port('debezium')}/connectors",
        json={
            "name": "mysql-connector",
            "config": {
                "connector.class": "io.debezium.connector.mysql.MySqlConnector",
                "database.hostname": "mysql",
                "database.port": "3306",
                "database.user": "root",
                "database.password": "rootpw",
                "database.server.name": "debezium",
                "database.server.id": "1234",
                "database.history.kafka.bootstrap.servers": "kafka:9092",
                "database.history.kafka.topic": "mysql-history",
                "database.allowPublicKeyRetrieval": "true",
                "time.precision.mode": "connect",
                "topic.prefix": "mysql",
            },
        },
    )
    # Don't error if the connector already exists.
    if response.status_code != requests.codes.conflict:
        response.raise_for_status()

    # Run load generator.
    c.run(
        "chbench",
        "run",
        "--config-file-path=/etc/chbenchmark/mz-default-mysql.cfg",
        "--dsn=mysql",
        "--gen-dir=/var/lib/mysql-files",
        "--analytic-threads=0",
        "--transactional-threads=1",
        "--run-seconds=86400",
        "--mz-sources",
        *unknown_args,
        detach=not args.wait,
    )


# invoked by ci/load
def workflow_load_test(c: Composition) -> None:
    """Run CH-benCHmark with a selected amount of load against Materialize."""
    c.workflow(
        "default",
        "--peek-conns=1",
        "--mz-views=q01,q02,q05,q06,q08,q09,q12,q14,q17,q19",
        "--transactional-threads=2",
    )
