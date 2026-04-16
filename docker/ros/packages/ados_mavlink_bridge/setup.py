from setuptools import find_packages, setup

package_name = "ados_mavlink_bridge"

setup(
    name=package_name,
    version="0.1.0",
    packages=find_packages(exclude=["test"]),
    data_files=[
        ("share/ament_index/resource_index/packages", ["resource/" + package_name]),
        ("share/" + package_name, ["package.xml"]),
        ("share/" + package_name + "/launch", ["launch/bridge.launch.py"]),
    ],
    install_requires=["setuptools", "pymavlink"],
    zip_safe=True,
    maintainer="Altnautica",
    maintainer_email="team@altnautica.com",
    description="ADOS MAVLink to ROS 2 bridge",
    license="GPL-3.0-only",
    entry_points={
        "console_scripts": [
            "bridge_node = ados_mavlink_bridge.bridge_node:main",
        ],
    },
)
