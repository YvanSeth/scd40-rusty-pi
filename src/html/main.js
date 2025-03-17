// data field names
var data = [ "co2ppm", "temperature", "humidity" ];

// when the DOM is ready use JS to remove the "enable JS" text
document.addEventListener("DOMContentLoaded", function() {
    data.forEach(( datum, i ) => document.getElementById(datum).innerHTML = "&hellip;");
}); 

var i1 = setInterval(
    function() {
        fetch("data/humidity")
            .then(function(response) { return response.json(); })
            .then(function(json) {
                document.getElementById("humidity").innerHTML = parseFloat(json).toFixed(0) + "%";
        	return fetch("data/temperature");
            })
            .then(function(response) { return response.json(); })
            .then(function(json) {
                document.getElementById("temperature").innerHTML = parseFloat(json).toFixed(1) + "&deg;C";
        	return fetch("data/co2ppm")
            })
            .then(function(response) { return response.json(); })
            .then(function(json) {
                document.getElementById("co2ppm").innerHTML = json + " PPM";
            });
    },
    2000
);
