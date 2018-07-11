import os
import socket, struct
import threading

import paramiko
from paramiko import SSHClient
from retry.api import retry_call


class SSHConnection:
    """
    SSHConnection hides the complexity of sending commands to the microVM
    via ssh.

    This class should be instantiated as part of the ssh fixture with the
    the hostname obtained from the MAC address, the username for logging into
    the image and the path of the ssh key.

    The ssh config dictionary contains the following fields:
    * hostname
    * username
    * ssh_key_path

    This translates in a ssh connection as follows:
    ssh -i ssh_key_path username@hostname
    """
    def __init__(self, ssh_config):
        self.ssh_client = SSHClient()
        self.ssh_client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
        assert (os.path.exists(ssh_config['ssh_key_path']))
        # Retry to connect to the host as long as the delay between calls
        # is less than 30 seconds. Sleep 1, 2, 4, 8, ... seconds between
        # attempts. These parameters might need additional tweaking when we
        # add tests with 1000 Firecracker microvm.
        retry_call(
            self.ssh_client.connect,
            fargs=[ssh_config['hostname']],
            fkwargs={
                "look_for_keys": False,
                "username": ssh_config['username'],
                "key_filename": ssh_config['ssh_key_path']
            },
            exceptions=paramiko.ssh_exception.NoValidConnectionsError,
            delay=1,
            backoff=2,
            max_delay=32
        )

    def execute_command(self, cmd_string):
        """ Executes the command passed as a string in the ssh context. """
        return self.ssh_client.exec_command(cmd_string)

    def close(self):
        """ Closes the SSH connection. """
        self.ssh_client.close()


class NoMoreIPsError(Exception):
    pass


class InvalidIPCount(Exception):
    pass


class SingletonReinitializationError(Exception):
    pass


class UniqueIPv4Generator:
    __instance = None

    @staticmethod
    def get_instance():
        """
        This class should be instantiated once per test session. All the
        microvms will have to use the same netmask length for
        the generator to work.

        This class will only generate IP addresses from the ranges
        192.168.0.0 - 192.168.255.255 and 172.16.0.0 - 172.31.255.255 which
        are the private IPs sub-networks.

        For a network mask of 30 bits, the UniqueIPv4Generator can generate up
        to 16320 sub-networks, each with 2 valid IPs from the
        192.168.0.0 - 192.168.255.255 range and 244800 sub-networks from the
        172.16.0.0 - 172.31.255.255 range.
        """
        if not UniqueIPv4Generator.__instance:
            return UniqueIPv4Generator()
        else:
            return UniqueIPv4Generator.__instance

    @staticmethod
    def __ip_to_int(ip: str):
        return int.from_bytes(socket.inet_aton(ip), "big")

    def __init__(self):
        """
        The init function should not be called directly. Use get_instance
        instead.
        """
        if self.__instance:
            raise SingletonReinitializationError

        # For the IPv4 address range 192.168.0.0 - 192.168.255.255, the mask
        # length is 16 bits. This means that the netmask_len used to
        # initialize the class can't be smaller that 16. For now we stick to
        # the default mask length = 30.
        self.NETMASK_LEN = 30
        self.ip_range = [
            ("192.168.0.0", "192.168.255.255"),
            ("172.16.0.0", "172.31.255.255")
        ]
        # We start by consuming IPs from the first defined range.
        self.ip_range_index = 0
        # The ip_range_min_index is the first IP in the range that can be used.
        # For the first range, this corresponds to "192.168.0.0".
        self.ip_range_min_index = 0

        # The ip_range_max_index is the last IP in the range that can be used.
        # For the first range, this corresponds to "192.168.255.255".
        self.ip_range_max_index = 1
        self.next_valid_subnet_id = self.__ip_to_int(
            self.ip_range[self.ip_range_index][0]
        )

        # The subnet_len contains the number of valid IPs in a subnet and it is
        # used to increment the next_valid_subnet_id once a request for a
        # subnet is issued.
        self.subnet_max_ip_count = (1 << 32 - self.NETMASK_LEN)

        self.lock = threading.Lock()

        UniqueIPv4Generator.__instance = self

    def __ensure_next_subnet(self):
        """ Raises Exception if there are no subnets available. """
        max_ip_as_int = self.__ip_to_int(
                self.ip_range[self.ip_range_index][self.ip_range_max_index]
        )

        """Check if there are any IPs left to use from the current range."""
        if (self.next_valid_subnet_id + self.subnet_max_ip_count) > max_ip_as_int:
            # Check if there are any other IP ranges.
            if self.ip_range_index < len(self.ip_range) - 1:
                # Move to the next IP range.
                self.ip_range_index += 1
                self.next_valid_subnet_id = self.__ip_to_int(
                    self.ip_range[self.ip_range_index][self.ip_range_min_index]
                )
            else:
                """
                There are no other ranges defined, so we don't have any IPs
                unassigned.
                """
                raise NoMoreIPsError

    def get_netmask_len(self):
        return self.NETMASK_LEN

    def get_next_available_subnet_range(self):
        """
        :return: range of IPs (defined as a pair) from an unused subnet.
         The mask used is the one defined when instantiating the
         UniqueIPv4Generator class.
        """
        with self.lock:
            self.__ensure_next_subnet()
            next_available_subnet = (
                socket.inet_ntoa(
                    struct.pack('!L', self.next_valid_subnet_id)
                ),
                socket.inet_ntoa(
                    struct.pack(
                        '!L',
                        self.next_valid_subnet_id +
                        (self.subnet_max_ip_count - 1)
                    )
                )
            )

            self.next_valid_subnet_id += self.subnet_max_ip_count
            return next_available_subnet

    def get_next_available_ips(self, count):
        """
        The function returns #count unique IPs. Raises InvalidIPCount when
        the requested IPs number is > than the length of the subnet mask -2.
        Two IPs from the subnet are reserved because the first address is
        the subnet identifier and the last IP is the broadcast IP.
        :param count: number of unique IPs to return
        :return: list of IPs as a list of strings
        """
        if count > self.subnet_max_ip_count - 2:
            raise InvalidIPCount

        with self.lock:
            self.__ensure_next_subnet()
            # The first IP in a subnet is the subnet identifier.
            next_available_ip = self.next_valid_subnet_id + 1
            ip_list = []
            for i in range(count):
                ip_as_string = socket.inet_ntoa(
                    struct.pack('!L', next_available_ip)
                )
                ip_list.append(ip_as_string)
                next_available_ip += 1
            self.next_valid_subnet_id += self.subnet_max_ip_count
            return ip_list


def mac_from_ip(ip_address):
    """
    The function creates a MAC address using the provided IP as follows:
    - the first 2 bytes are fixed to 06:00
    - the next 4 bytes are the IP address
    Example of function call:
    mac_from_ip("192.168.241.2") -> 06:00:C0:A8:F1:02
    C0 = 192, A8 = 168, F1 = 241 and  02 = 2
    :param ip_address: IP address as string
    :return: MAC address from IP
    """
    mac_as_list = ["06", "00"]
    mac_as_list.extend(
        list(map(lambda val: '{0:02x}'.format(int(val)), ip_address.split('.')))
    )

    return "{}:{}:{}:{}:{}:{}".format(*mac_as_list)
