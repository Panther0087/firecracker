# Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests that ensure the boot time to init process is within spec."""

import os
import re
import time

from framework import decorators

import host_tools.logging as log_tools

# The maximum acceptable boot time in us.
MAX_BOOT_TIME_US = 150000
# TODO: Keep a `current` boot time in S3 and validate we don't regress
# Regex for obtaining boot time from some string.
TIMESTAMP_LOG_REGEX = r'Guest-boot-time\s+\=\s+(\d+)\s+us'

NO_OF_MICROVMS = 10


def test_single_microvm_boottime_no_network(test_microvm_with_boottime):
    """Check guest boottime of microvm without network."""
    log_fifo, _ = _configure_vm(test_microvm_with_boottime)
    time.sleep(0.4)
    boottime_us = _test_microvm_boottime(log_fifo)
    print("Boot time with no network is: " + str(boottime_us) + " us")


@decorators.test_context('boottime', NO_OF_MICROVMS)
def test_multiple_microvm_boottime_no_network(test_multiple_microvms):
    """Check guest boottime without network when spawning multiple microvms."""
    microvms = test_multiple_microvms
    assert microvms
    assert len(microvms) == NO_OF_MICROVMS
    log_fifos = []
    for i in range(NO_OF_MICROVMS):
        log_fifo, _ = _configure_vm(microvms[i])
        log_fifos.append(log_fifo)
    time.sleep(0.4)
    for i in range(NO_OF_MICROVMS):
        _ = _test_microvm_boottime(log_fifos[i])


@decorators.test_context('boottime', NO_OF_MICROVMS)
def test_multiple_microvm_boottime_with_network(
        test_multiple_microvms,
        network_config
):
    """Check guest boottime with network when spawning multiple microvms."""
    microvms = test_multiple_microvms
    assert microvms
    assert len(microvms) == NO_OF_MICROVMS
    log_fifos = []
    _taps = []
    for i in range(NO_OF_MICROVMS):
        log_fifo, _tap = _configure_vm(microvms[i], {
            "config": network_config, "iface_id": str(i)
        })
        log_fifos.append(log_fifo)
        _taps.append(_tap)
    time.sleep(0.4)
    for i in range(NO_OF_MICROVMS):
        _ = _test_microvm_boottime(log_fifos[i])


def test_single_microvm_boottime_with_network(
        test_microvm_with_boottime,
        network_config
):
    """Check guest boottime of microvm with network."""
    log_fifo, _tap = _configure_vm(test_microvm_with_boottime, {
        "config": network_config, "iface_id": "1"
    })
    time.sleep(0.4)
    boottime_us = _test_microvm_boottime(log_fifo)
    print("Boot time with network configured is: " + str(boottime_us) + " us")


def _test_microvm_boottime(log_fifo):
    """Assert that we meet the minimum boot time.

    TODO: Should use a microVM with the `boottime` capability.
    """
    lines = log_fifo.sequential_reader(20)

    boot_time_us = 0
    for line in lines:
        timestamps = re.findall(TIMESTAMP_LOG_REGEX, line)
        if timestamps:
            boot_time_us = int(timestamps[0])

    assert boot_time_us > 0
    assert boot_time_us < MAX_BOOT_TIME_US
    return boot_time_us


def _configure_vm(microvm, network_info=None):
    """Auxiliary function for preparing microvm before measuring boottime."""
    microvm.spawn()

    # Machine configuration specified in the SLA.
    microvm.basic_config(
        vcpu_count=1,
        mem_size_mib=128
    )
    if network_info:
        _tap, _, _ = microvm.ssh_network_config(
            network_info["config"],
            network_info["iface_id"]
        )

    # Configure logging.
    log_fifo_path = os.path.join(
        microvm.path,
        'log_fifo' + microvm.id.split('-')[0]
    )
    metrics_fifo_path = os.path.join(
        microvm.path,
        'metrics_fifo' + microvm.id.split('-')[0]
    )
    log_fifo = log_tools.Fifo(log_fifo_path)
    metrics_fifo = log_tools.Fifo(metrics_fifo_path)

    response = microvm.logger.put(
        log_fifo=microvm.create_jailed_resource(log_fifo.path),
        metrics_fifo=microvm.create_jailed_resource(metrics_fifo.path),
        level='Warning',
        show_level=False,
        show_log_origin=False
    )
    assert microvm.api_session.is_good_response(response.status_code)

    microvm.start()
    return log_fifo, _tap if network_info else None
