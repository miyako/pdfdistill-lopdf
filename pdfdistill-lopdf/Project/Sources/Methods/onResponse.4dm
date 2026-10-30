//%attributes = {}
#DECLARE($worker : 4D:C1709.SystemWorker; $params : Object)

var $text : Text
$text:=$worker.responseError  //information
ALERT:C41($text)
$file:=$params.context  //the distilled pdf