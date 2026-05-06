# BR2_EXTERNAL tree for the ADOS Drone Agent lite profile.
#
# Pulls every per-package recipe under package/ into the Buildroot tree.
# Add new recipes by dropping a folder under package/ and including its
# .mk via the include line below.

include $(sort $(wildcard $(BR2_EXTERNAL_ADOS_PATH)/package/*/*.mk))
