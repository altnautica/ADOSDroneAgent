from setuptools import find_packages, setup

package_name = "ados_foxglove_launch"

setup(
    name=package_name,
    version="0.1.0",
    packages=find_packages(exclude=["test"]),
    data_files=[
        ("share/ament_index/resource_index/packages", ["resource/" + package_name]),
        ("share/" + package_name, ["package.xml"]),
        ("share/" + package_name + "/launch", ["launch/foxglove.launch.py"]),
    ],
    install_requires=["setuptools"],
    zip_safe=True,
    maintainer="Altnautica",
    maintainer_email="team@altnautica.com",
    description="Foxglove bridge launch config for ADOS",
    license="GPL-3.0-only",
)
