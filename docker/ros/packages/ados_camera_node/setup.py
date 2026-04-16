from setuptools import find_packages, setup

package_name = "ados_camera_node"

setup(
    name=package_name,
    version="0.1.0",
    packages=find_packages(exclude=["test"]),
    data_files=[
        ("share/ament_index/resource_index/packages", ["resource/" + package_name]),
        ("share/" + package_name, ["package.xml"]),
        ("share/" + package_name + "/launch", ["launch/camera.launch.py"]),
    ],
    install_requires=["setuptools"],
    zip_safe=True,
    maintainer="Altnautica",
    maintainer_email="team@altnautica.com",
    description="USB UVC camera capture for ADOS ROS 2 environment",
    license="GPL-3.0-only",
    entry_points={
        "console_scripts": [
            "camera_node = ados_camera_node.camera_node:main",
        ],
    },
)
