# Configuration file for VirtualBox, it creates a VirtualBox virtual machine

virtualbox: $(BUILD)/harddrive.img
	echo "Delete VM"
	-$(VBM) unregistervm Absolution --delete; \
	if [ $$? -ne 0 ]; \
	then \
		if [ -d "$$HOME/VirtualBox VMs/Absolution" ]; \
		then \
			echo "Absolution directory exists, deleting..."; \
			$(RM) -rf "$$HOME/VirtualBox VMs/Absolution"; \
		fi \
	fi
	echo "Delete Disk"
	-$(RM) harddrive.vdi
	echo "Create VM"
	$(VBM) createvm --name Absolution --register
	echo "Set Configuration"
	$(VBM) modifyvm Absolution --memory 2048
	$(VBM) modifyvm Absolution --vram 32
	if [ "$(net)" != "no" ]; \
	then \
		$(VBM) modifyvm Absolution --nic1 nat; \
		$(VBM) modifyvm Absolution --nictype1 82540EM; \
		$(VBM) modifyvm Absolution --cableconnected1 on; \
		$(VBM) modifyvm Absolution --nictrace1 on; \
		$(VBM) modifyvm Absolution --nictracefile1 "$(ROOT)/$(BUILD)/network.pcap"; \
	fi
	$(VBM) modifyvm Absolution --uart1 0x3F8 4
	$(VBM) modifyvm Absolution --uartmode1 file "$(ROOT)/$(BUILD)/serial.log"
	$(VBM) modifyvm Absolution --usb off # on
	$(VBM) modifyvm Absolution --keyboard ps2
	$(VBM) modifyvm Absolution --mouse ps2
	$(VBM) modifyvm Absolution --audio-driver $(VB_AUDIO)
	$(VBM) modifyvm Absolution --audiocontroller hda
	$(VBM) modifyvm Absolution --audioout on
	$(VBM) modifyvm Absolution --nestedpaging on
	echo "Create Disk"
	$(VBM) convertfromraw $< $(BUILD)/harddrive.vdi
	echo "Attach Disk"
	$(VBM) storagectl Absolution --name ATA --add sata --controller IntelAHCI --bootable on --portcount 1
	$(VBM) storageattach Absolution --storagectl ATA --port 0 --device 0 --type hdd --medium $(BUILD)/harddrive.vdi
	echo "Run VM"
	$(VBM) startvm Absolution
