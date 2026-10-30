property stdOut : Text
property stdErr : Text

Class extends _CLI_Controller

Class constructor($CLI : cs:C1710._CLI)
	
	Super:C1705($CLI)
	
	This:C1470.clear()
	
Function clear() : cs:C1710._distill_Controller
	
	This:C1470.stdOut:=""
	This:C1470.stdErr:=""
	
	return This:C1470
	
Function onData($worker : 4D:C1709.SystemWorker; $params : Object)
	
	This:C1470.stdOut+=$params.data
	
Function onDataError($worker : 4D:C1709.SystemWorker; $params : Object)
	
	This:C1470.stdErr+=$params.data
	
Function onResponse($worker : 4D:C1709.SystemWorker; $params : Object)
	
	var $instance : cs:C1710.distill
	$instance:=This:C1470.instance
	
	If (OB Instance of:C1731($instance.onResponse; 4D:C1709.Function))
		$instance.onResponse.call($worker; $params)
	End if 
	
Function onError($worker : 4D:C1709.SystemWorker; $params : Object)
	
	var $instance : cs:C1710.distill
	$instance:=This:C1470.instance
	
	If (OB Instance of:C1731($instance.onError; 4D:C1709.Function))
		$instance.onError.call($worker; $params)
	End if 
	
Function onTerminate($worker : 4D:C1709.SystemWorker; $params : Object)
	
	var $instance : cs:C1710.distill
	$instance:=This:C1470.instance
	
	If (OB Instance of:C1731($instance.onTerminate; 4D:C1709.Function))
		$instance.onTerminate.call($worker; $params)
	End if 